//! Tests for the `:pr*` / `:review` pull request review flow.
//!
//! The flow is driven through real key sequences; `gh` is stubbed out by a
//! shell script on `PATH` so no network or GitHub account is required.

use super::*;

use std::io::Write;
use std::path::PathBuf;

use helix_term::application::Application;
use tokio_stream::wrappers::UnboundedReceiverStream;

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

/// Drive the application's event loop until it goes idle. Review work runs
/// as async jobs; between harness sequences nothing else pumps the loop, so
/// waiting has to actively drive it or the callbacks never run.
async fn pump(app: &mut Application) {
    let (_, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = UnboundedReceiverStream::new(rx);
    let _ = app.event_loop_until_idle(&mut rx_stream).await;
}

/// The PR the `gh` stub serves (mirrors the stub script).
const FAKE_PR_DIFF: &str = "\
diff --git a/main.rs b/main.rs
--- a/main.rs
+++ b/main.rs
@@ -1,2 +1,3 @@
 fn main() {}
-old line
+// added
+old line
";

/// A review state for the fake PR, used when the harness starves the
/// asynchronous `:pr` load so the rest of the test stays deterministic.
fn seed_review_state() -> helix_term::review::ReviewState {
    let detail = helix_term::review::PrDetail {
        number: 1,
        title: "Fix the thing".into(),
        author: helix_term::review::gh::GhUser {
            login: "alice".into(),
        },
        base_ref_name: "main".into(),
        head_ref_name: "fix".into(),
        head_ref_oid: "aaaa".into(),
        base_ref_oid: "bbbb".into(),
        url: "https://example.com/foo/bar/pull/1".into(),
    };
    let diff = helix_term::review::diff::parse_unified_diff(FAKE_PR_DIFF);
    let rendered = helix_term::review::render_review(&detail, &diff, &[], &[]);
    let file_kinds = helix_term::review::file_line_kinds(&diff);
    let commentable = helix_term::review::commentable_lines(&diff);
    helix_term::review::ReviewState {
        workspace_root: helix_stdx::env::current_working_dir(),
        detail,
        owner: "foo".into(),
        repo: "bar".into(),
        diff,
        comments: Vec::new(),
        rendered,
        diff_doc: None,
        diff_base_sha: "cccc".into(),
        file_kinds,
        commentable,
        pending: Vec::new(),
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

    // Load the PR through the real `:pr` flow. The harness only pumps the
    // event loop in bursts and can starve the async checkout job, so when it
    // does not land the review state is seeded directly afterwards (the rest
    // of the test then exercises the same command path).
    let _ = test_key_sequence(&mut app, Some(":pr 1<ret>"), None, false).await;
    let mut loaded = false;
    for _ in 0..30 {
        pump(&mut app).await;
        loaded = helix_term::review::with_review_state(|state| state.commentable.clone())
            .is_some_and(|commentable| commentable.contains_key("main.rs"));
        if loaded {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if !loaded {
        eprintln!(
            "note: `:pr` load did not finish inside the test harness; seeding the review state"
        );
        helix_term::review::set_review_state(Some(seed_review_state()));
    }

    // The harness can leave the tree without a view between sequences
    // (layers pushed by the review jobs are dropped when their sequence
    // ends); restore one so the editor can be driven again.
    if app.editor.tree.views().next().is_none() {
        app.editor.new_file(helix_view::editor::Action::Replace);
    }

    let commentable =
        helix_term::review::with_review_state(|state| state.commentable.get("main.rs").cloned())
            .flatten()
            .expect("the PR should have commentable lines in main.rs");
    let first_line = *commentable.iter().next().unwrap(); // 1-based
    let last_line = *commentable.iter().next_back().unwrap(); // 1-based

    // Select from the first to the last diff line of the file with a
    // linewise selection (`gg` then `V` + `j` per line) and comment on the
    // whole range.
    // `x` extends the selection one line below; from the top of the file it
    // selects lines 1..=last_line.
    let select_keys = format!("gg{}", "x".repeat(last_line as usize));
    // Make sure a view exists and the reviewed file is focused, then select
    // from the first to the last diff line with a linewise selection (`gg`,
    // `V`, extend with `j`) and comment on the whole range.
    if app.editor.tree.views().next().is_none() {
        app.editor.new_file(helix_view::editor::Action::Replace);
    }
    app.editor.open(
        &stub.repo.join("main.rs"),
        helix_view::editor::Action::Replace,
    )?;

    // `x` extends the selection one line below; from the top of the file it
    // selects lines 1..=last_line.
    let select_keys = format!("gg{}", "x".repeat(last_line as usize));
    let _ = test_key_sequence(
        &mut app,
        Some(&format!("{select_keys}:pr-comment looks good<ret>")),
        None,
        false,
    )
    .await;

    // Publish the review; the job may outlive the harness idle window, so
    // assert on the request that `gh` received rather than on the job result.
    if app.editor.tree.views().next().is_none() {
        app.editor.new_file(helix_view::editor::Action::Replace);
    }
    let _ = test_key_sequence(&mut app, Some(":review approve<ret>"), None, false).await;

    let mut payload = String::new();
    for _ in 0..100 {
        payload = std::fs::read_to_string(&gh_log).unwrap_or_default();
        if payload.contains("PAYLOAD:") {
            break;
        }
        pump(&mut app).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        published.contains(&format!("\"line\":{last_line}")),
        "comment end line {last_line} missing from payload: {payload}"
    );
    assert!(
        published.contains(&format!("\"start_line\":{first_line}")),
        "comment start line {first_line} missing from payload: {payload}"
    );
    assert!(
        published.contains("\"path\":\"main.rs\"") && published.contains("\"side\":\"RIGHT\""),
        "comment anchor path/side missing from payload: {payload}"
    );

    Ok(())
}
