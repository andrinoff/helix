//! Thin asynchronous wrapper around the `gh` CLI plus the JSON payload types
//! used for pull request review.
//!
//! All GitHub interaction goes through `gh` so that authentication (tokens,
//! `gh auth login`, enterprise hosts) is handled entirely outside of helix.
//! Every invocation runs in the workspace root, disables interactive
//! prompts via `GH_PROMPT_DISABLED`, and is subject to a timeout so a hung
//! `gh` can never freeze the editor.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde::Deserialize;
use tokio::process::Command;

const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `gh` with the given arguments and return its stdout.
pub async fn run_gh<S: AsRef<OsStr> + std::fmt::Debug>(
    cwd: &Path,
    args: &[S],
) -> anyhow::Result<String> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .current_dir(cwd)
        // Never let gh block the editor on an interactive question.
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::time::timeout(GH_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow!("gh did not respond within {} seconds", GH_TIMEOUT.as_secs()))?
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "the `gh` CLI was not found on PATH; install it from \
                     https://cli.github.com and log in with `gh auth login`"
                )
            } else {
                anyhow!("failed to run `gh`: {err}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!("`gh {args:?}` failed: {}", error_text(&stderr, &stdout));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Human-friendly message for a failed `gh` run: the GitHub REST API reports
/// errors as JSON on stderr, so unwrap the common `{"message": ...}` shape.
pub(crate) fn error_text(stderr: &str, stdout: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(stderr)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(|message| message.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| stderr.to_string());
    if message.is_empty() {
        stdout.to_string()
    } else {
        message
    }
}

/// Author (login) of a PR or comment.
#[derive(Debug, Clone, Deserialize)]
pub struct GhUser {
    pub login: String,
}

/// Entry of `gh pr list --json ...`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrListItem {
    pub number: u64,
    pub title: String,
    pub author: GhUser,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub updated_at: String,
}

/// Result of `gh pr view <n> --json ...`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetail {
    pub number: u64,
    pub title: String,
    pub author: GhUser,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_oid: String,
    pub url: String,
}

/// Review comment from the REST API
/// (`GET /repos/{owner}/{repo}/pulls/{number}/comments`).
#[derive(Debug, Clone, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    #[serde(default)]
    pub body: String,
    /// Repository-relative path the comment is anchored to.
    #[serde(default)]
    pub path: Option<String>,
    /// Line in the current diff side, if the comment is still anchored.
    #[serde(default)]
    pub line: Option<u32>,
    /// Line in the diff as it was when the comment was written.
    #[serde(default)]
    pub original_line: Option<u32>,
    /// `RIGHT` (new file) or `LEFT` (old file).
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(default)]
    pub user: Option<GhUser>,
    #[serde(default)]
    pub created_at: String,
}

impl ReviewComment {
    pub fn login(&self) -> &str {
        self.user
            .as_ref()
            .map(|user| user.login.as_str())
            .unwrap_or("ghost")
    }

    /// The `(path, side, line)` this comment anchors to, if it has one.
    pub fn anchor(&self) -> Option<(&str, Side, u32)> {
        let side = match self.side.as_deref() {
            Some("LEFT") => Side::Left,
            _ => Side::Right,
        };
        // `line` refers to the current diff version; fall back to the
        // original line for comments whose anchor has since moved.
        let line = self.line.or(self.original_line)?;
        let path = self.path.as_deref()?;
        Some((path, side, line))
    }
}

/// Which side of the diff an anchor or comment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The new (head) version of the file.
    Right,
    /// The old (base) version of the file.
    Left,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Right => "RIGHT",
            Side::Left => "LEFT",
        }
    }
}

/// Extract `(owner, repo)` from a PR URL like
/// `https://github.com/helix-editor/helix/pull/1`.
pub fn owner_repo_from_url(url: &str) -> Option<(String, String)> {
    let url = url.trim_end_matches('/');
    let (base, _) = url.rsplit_once("/pull/")?;
    let parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    let repo = *parts.last()?;
    let owner = *parts.get(parts.len().checked_sub(2)?)?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Fetch all review comments of a PR via the REST API.
pub async fn fetch_comments(
    cwd: &Path,
    owner: &str,
    repo: &str,
    number: u64,
) -> anyhow::Result<Vec<ReviewComment>> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
    let output = run_gh(cwd, &["api", "--paginate", &endpoint]).await?;
    let comments: Vec<ReviewComment> = serde_json::from_str(&output)
        .map_err(|err| anyhow!("could not parse review comments: {err}"))?;
    Ok(comments)
}

/// A new review comment to create on the PR head commit.
#[derive(Debug, Clone)]
pub struct NewComment {
    pub commit_id: String,
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub body: String,
}

/// Create a review comment on `(path, side, line)` of the PR head commit.
pub async fn post_comment(
    cwd: &Path,
    owner: &str,
    repo: &str,
    number: u64,
    comment: &NewComment,
) -> anyhow::Result<ReviewComment> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
    let mut args: Vec<String> = vec![
        "api".into(),
        "-X".into(),
        "POST".into(),
        endpoint,
        "-f".into(),
        format!("body={}", comment.body),
        "-f".into(),
        format!("commit_id={}", comment.commit_id),
        "-f".into(),
        format!("path={}", comment.path),
        "-F".into(),
        format!("line={}", comment.line),
    ];
    if comment.side == Side::Left {
        args.push("-f".into());
        args.push("side=LEFT".into());
    }
    let output = run_gh(cwd, &args).await?;
    let created: ReviewComment = serde_json::from_str(&output)
        .map_err(|err| anyhow!("could not parse created comment: {err}"))?;
    Ok(created)
}

/// The commit to diff PR files against. GitHub compares the head against the
/// merge base of the head and base branches (three-dot), which is what `gh pr
/// diff` displays too.
pub async fn fetch_diff_base_sha(
    cwd: &Path,
    owner: &str,
    repo: &str,
    head_sha: &str,
    base_sha: &str,
) -> anyhow::Result<String> {
    let endpoint = format!("repos/{owner}/{repo}/compare/{head_sha}...{base_sha}");
    let output = run_gh(cwd, &["api", "--jq", ".merge_base_commit.sha", &endpoint]).await?;
    let sha = output.trim().to_string();
    if sha.is_empty() {
        Ok(base_sha.to_string())
    } else {
        Ok(sha)
    }
}

/// Base-branch content of a file (raw bytes). An empty vec means the file did
/// not exist at the base commit (i.e. the PR adds it).
pub async fn fetch_base_content(
    cwd: &Path,
    owner: &str,
    repo: &str,
    path: &str,
    base_sha: &str,
) -> anyhow::Result<Vec<u8>> {
    let endpoint = format!("repos/{owner}/{repo}/contents/{path}?ref={base_sha}");
    let output = match run_gh(cwd, &["api", &endpoint]).await {
        Ok(output) => output,
        Err(err) => {
            // A 404 means the PR adds the file: an empty base marks every
            // line as added.
            if err.to_string().contains("Not Found") {
                return Ok(Vec::new());
            }
            return Err(err);
        }
    };
    let value: serde_json::Value = serde_json::from_str(&output)?;
    let content = value
        .get("content")
        .and_then(|content| content.as_str())
        .ok_or_else(|| anyhow!("could not parse the file contents response"))?;
    decode_base64(content)
}

/// Minimal standard-alphabet base64 decoder for the contents API response.
fn decode_base64(input: &str) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for character in input.chars().filter(|c| !c.is_whitespace()) {
        let value = match character {
            'A'..='Z' => character as u32 - 'A' as u32,
            'a'..='z' => character as u32 - 'a' as u32 + 26,
            '0'..='9' => character as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ => bail!("invalid base64 character '{character}'"),
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_repo() {
        assert_eq!(
            owner_repo_from_url("https://github.com/helix-editor/helix/pull/16090"),
            Some(("helix-editor".to_string(), "helix".to_string()))
        );
        assert_eq!(
            owner_repo_from_url("https://github.com/foo/bar/pull/12/"),
            Some(("foo".to_string(), "bar".to_string()))
        );
        assert_eq!(owner_repo_from_url("https://example.com/nope"), None);
        assert_eq!(owner_repo_from_url("https://github.com/onlyowner"), None);
    }

    #[test]
    fn parses_comment_anchor() {
        let mut comment = ReviewComment {
            id: 1,
            body: "hi".into(),
            path: Some("src/lib.rs".into()),
            line: Some(42),
            original_line: Some(40),
            side: Some("RIGHT".into()),
            in_reply_to_id: None,
            user: None,
            created_at: String::new(),
        };
        assert_eq!(comment.anchor(), Some(("src/lib.rs", Side::Right, 42)));

        comment.side = Some("LEFT".into());
        comment.line = None;
        assert_eq!(comment.anchor(), Some(("src/lib.rs", Side::Left, 40)));

        comment.path = None;
        assert_eq!(comment.anchor(), None);
    }

    #[test]
    fn deserializes_comment_json() {
        let json = r#"[
            {
                "id": 801,
                "body": "Looks good to me",
                "path": "src/main.rs",
                "line": 12,
                "side": "RIGHT",
                "user": {"login": "alice"},
                "created_at": "2023-01-01T00:00:00Z"
            }
        ]"#;
        let comments: Vec<ReviewComment> = serde_json::from_str(json).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].login(), "alice");
        assert_eq!(comments[0].anchor(), Some(("src/main.rs", Side::Right, 12)));
    }

    #[test]
    fn deserializes_pr_detail() {
        let json = r#"{
            "number": 12,
            "title": "Fix crash",
            "author": {"login": "bob"},
            "baseRefName": "main",
            "headRefName": "fix",
            "headRefOid": "0123456789abcdef",
            "baseRefOid": "fedcba9876543210",
            "url": "https://github.com/foo/bar/pull/12"
        }"#;
        let detail: PrDetail = serde_json::from_str(json).unwrap();
        assert_eq!(detail.number, 12);
        assert_eq!(detail.base_ref_name, "main");
        assert_eq!(detail.base_ref_oid, "fedcba9876543210");
        assert_eq!(
            owner_repo_from_url(&detail.url),
            Some(("foo".into(), "bar".into()))
        );
    }

    #[test]
    fn decodes_base64() {
        // RFC 4648 test vectors.
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode_base64("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        // The API may fold the base64 across lines.
        assert_eq!(decode_base64("Zm9v\nYmFy").unwrap(), b"foobar",);
        assert!(decode_base64("Zm9!v").is_err());
    }
}
