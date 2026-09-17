//! Tests for the `:pr*` / `:review` pull request review flow.
//!
//! The flow is driven through real key sequences; `gh` is stubbed out by a
//! shell script on `PATH` so no network or GitHub account is required.

use super::*;

use std::io::Write;
use std::path::PathBuf;

/// A `gh` stub plus a git repository containing the changes of the fake PR.
/// The returned guard keeps the temp dir alive for the duration of the test.
struct GhStub {
    _dir: tempfile::TempDir,
    repo: PathBuf,
}

impl GhStub {
    fn new() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo)?;

        // Minimal git repository on `main`.
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("git must be installed to run this test");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        // Signing would need a passphrase/GPG agent that CI and the test
        // environment do not have.
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "tag.gpgsign", "false"]);
        std::fs::write(repo.join("main.rs"), "fn main() {}\nold line\n")?;
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        git(&["remote", "add", "origin", "https://example.com/foo/bar.git"]);

        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin)?;
        let gh = bin.join("gh");
        let mut file = std::fs::File::create(&gh)?;
        // The stub answers the handful of `gh` calls the review flow makes.
        // `pr checkout` rewrites the working tree, mirroring what a real
        // checkout of the fake PR would do.
        write!(
            file,
            r#"#!/bin/sh
echo "CALLED: $*" >> "$HELIX_TEST_GH_LOG"
case "$1 $2" in
  "pr view")
    cat <<JSON
{{"number":1,"title":"Fix the thing","author":{{"login":"alice"}},"baseRefName":"main","headRefName":"fix","headRefOid":"aaaa","baseRefOid":"bbbb","url":"https://example.com/foo/bar/pull/1"}}
JSON
    ;;
  "pr diff")
    cat <<DIFF
diff --git a/main.rs b/main.rs
--- a/main.rs
+++ b/main.rs
@@ -1,2 +1,3 @@
 fn main() {{}}
-old line
+// added
+old line
DIFF
    ;;
  "pr checkout")
    git checkout -q -b fix
    printf 'fn main() {{}}\n// added\nold line\n' > main.rs
    ;;
  "pr list")
    cat <<JSON
[{{"number":1,"title":"Fix the thing","author":{{"login":"alice"}},"headRefName":"fix","baseRefName":"main","updatedAt":"2026-01-01T00:00:00Z"}}]
JSON
    ;;
esac
case "$1" in
  api)
    case "$2" in
      --jq) echo "cccc" ;;
      --paginate) echo "[]" ;;
      --method)
        # POST /reviews: the JSON payload arrives on stdin.
        payload=$(cat)
        printf 'PAYLOAD:%s\n' "$payload" >> "$HELIX_TEST_GH_LOG"
        echo '{{"id":42}}'
        ;;
      *)
        printf '{{"name":"main.rs","encoding":"base64","content":"%s"}}' \
          "$(printf 'fn main() {{}}\nold line\n' | base64)"
        ;;
    esac
    ;;
esac
exit 0
"#
        )?;
        file.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
        }
        // Prepend the stub dir to PATH.
        let path = std::env::var("PATH").unwrap_or_default();
        let bin: PathBuf = bin;
        std::env::set_var("PATH", format!("{}:{}", bin.display(), path));

        Ok(Self { _dir: dir, repo })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_comment_and_review_submission() -> anyhow::Result<()> {
    if cfg!(not(unix)) {
        return Ok(());
    }
    let stub = GhStub::new()?;
    // The stub appends every invocation, and the payload of a published
    // review, to this file.
    let gh_log = PathBuf::from("/tmp/helix-review-gh.log");
    let _ = std::fs::remove_file(&gh_log);
    std::env::set_var("HELIX_TEST_GH_LOG", &gh_log);

    helix_stdx::env::set_current_working_dir(&stub.repo)?;
    let mut app = AppBuilder::new()
        .with_file(stub.repo.join("main.rs"), None)
        .build()?;

    // Load the PR (async checkout job; the harness drains it before the
    // sequence returns).
    test_key_sequence(
        &mut app,
        Some(":pr 1<ret>"),
        Some(&|app| {
            let status = app
                .editor
                .get_status()
                .map(|(status, _)| status.to_string());
            assert!(
                status.as_deref().unwrap_or_default().contains("PR #1"),
                "PR was not loaded: {status:?}"
            );
        }),
        false,
    )
    .await?;

    // The checkout job may still be running when the first sequence returns
    // (its keys are all queued up front). Wait until the review state exists
    // before driving the rest of the flow.
    for _ in 0..50 {
        if helix_term::review::with_review_state(|state| state.diff_base_sha.clone()).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // The harness drops compositor layers between sequences and can leave the
    // tree without a view; restore one so the editor can be driven again.
    if app.editor.tree.views().next().is_none() {
        app.editor.new_file(helix_view::editor::Action::Replace);
    }

    // The checkout runs as an async job whose keys were already queued, so
    // wait for the review state to appear before inspecting it.
    let mut anchor_line = None;
    for _ in 0..100 {
        anchor_line = helix_term::review::with_review_state(|state| {
            state
                .commentable
                .get("main.rs")
                .and_then(|lines| lines.iter().next().copied())
        })
        .flatten();
        if anchor_line.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let anchor_line = anchor_line.expect("the PR should have commentable lines in main.rs");

    // `test_key_sequence` returns `Err` when the editor does not go idle
    // within its window; the review jobs can exceed that, so the state is
    // asserted separately below.
    let _ = test_key_sequence(
        &mut app,
        Some(&format!(
            ":open {}<ret>:pr-comment looks good<ret>",
            stub.repo.join("main.rs").display()
        )),
        None,
        false,
    )
    .await;
    let pending = helix_term::review::with_review_state(|state| {
        state
            .pending
            .iter()
            .map(|comment| comment.body.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        pending.as_deref(),
        Some(&["looks good".to_string()][..]),
        "pending comment was not recorded: {pending:?}"
    );

    // Publish the review; the job may outlive the harness idle window, so
    // assert on the request that `gh` received rather than on the job result.
    let _ = test_key_sequence(&mut app, Some(":review approve<ret>"), None, false).await;

    let mut payload = String::new();
    for _ in 0..100 {
        payload = std::fs::read_to_string(&gh_log).unwrap_or_default();
        if payload.contains("PAYLOAD:") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let published = payload
        .lines()
        .find_map(|line| line.strip_prefix("PAYLOAD:"))
        .unwrap_or("");
    assert!(
        published.contains("\"event\":\"APPROVE\""),
        "review event missing from payload: {payload}"
    );
    assert!(
        published.contains("\"body\":\"looks good\""),
        "pending comment body missing from payload: {payload}"
    );
    assert!(
        published.contains(&format!("\"line\":{anchor_line}")),
        "comment anchor line {anchor_line} missing from payload: {payload}"
    );
    assert!(
        published.contains("\"path\":\"main.rs\"") && published.contains("\"side\":\"RIGHT\""),
        "comment anchor path/side missing from payload: {payload}"
    );

    Ok(())
}
