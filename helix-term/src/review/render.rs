//! Generation of the review (diff) buffer and its full-line highlights.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use helix_view::graphics::Rect;
use helix_view::theme::{Style, Theme};
use helix_view::DocumentId;

use crate::ui::{Decoration, LinePos, TextRenderer};

use super::diff::{DiffLineKind, FileStatus, PrDiff, PrDiffLine, PrFileDiff, PrHunk};
use super::gh::{PrDetail, ReviewComment, Side};
use super::{with_review_state, Anchor, LineKind, Rendered};

/// Maximum width of the header / separator lines.
const RULER_WIDTH: usize = 78;

/// Build the review buffer text for a PR.
///
/// The layout is:
///
/// ```text
/// PR #123: Fix the thing
/// Author: @alice    main <- fix-thing
/// https://github.com/owner/repo/pull/123
/// ──────────────────────────────────────
/// ── src/main.rs (modified) ────────────
/// @@ -12,7 +12,7 @@ fn main() {
/// -    let old = 1;
/// +    let new = 2;
///     ┌ @alice: should this be 3?
///     └
/// ```
///
/// Comment blocks are inserted as real lines below the diff line they anchor
/// to; comments that do not match any line of the current diff are collected
/// in a trailing section.
pub fn render_review(detail: &PrDetail, diff: &PrDiff, comments: &[ReviewComment]) -> Rendered {
    let mut text = String::new();
    let mut line_kinds = Vec::new();
    let mut line_anchors = Vec::new();

    let mut push = |line: String, kind: LineKind, anchor: Option<Anchor>| {
        text.push_str(&line);
        text.push('\n');
        line_kinds.push(kind);
        line_anchors.push(anchor);
    };

    // Header block
    push(
        format!("PR #{}: {}", detail.number, detail.title),
        LineKind::Header,
        None,
    );
    push(
        format!(
            "Author: @{}    {} ← {}",
            detail.author.login, detail.base_ref_name, detail.head_ref_name
        ),
        LineKind::Header,
        None,
    );
    push(detail.url.clone(), LineKind::Header, None);
    push("─".repeat(RULER_WIDTH), LineKind::Header, None);

    // Index comments by their anchor for O(1) lookup while rendering lines.
    let mut comment_map: HashMap<(String, Side, u32), Vec<usize>> = HashMap::new();
    for (index, comment) in comments.iter().enumerate() {
        if let Some((path, side, line)) = comment.anchor() {
            comment_map
                .entry((path.to_string(), side, line))
                .or_default()
                .push(index);
        }
    }
    let mut placed: HashSet<usize> = HashSet::new();

    for file in &diff.files {
        push(file_header(file), LineKind::Header, None);

        if file.status == FileStatus::Binary {
            push(
                "    (binary file, not shown)".into(),
                LineKind::Context,
                None,
            );
            continue;
        }

        for hunk in &file.hunks {
            push(hunk_header(hunk), LineKind::Header, None);
            for line in &hunk.lines {
                let (rendered, anchor) = render_diff_line(file, line);
                let key = anchor
                    .as_ref()
                    .map(|anchor| (anchor.path.clone(), anchor.side, anchor.line));
                push(rendered, kind_of(line.kind), anchor);
                if let Some(key) = key {
                    if let Some(indices) = comment_map.get(&key) {
                        for &index in indices {
                            placed.insert(index);
                            push_comment_block(&mut push, &comments[index]);
                        }
                    }
                }
            }
        }
    }

    // Outdated comments: anchored to a diff version that no longer matches.
    let outdated: Vec<&ReviewComment> = comments
        .iter()
        .enumerate()
        .filter(|(index, _)| !placed.contains(index))
        .map(|(_, comment)| comment)
        .collect();
    if !outdated.is_empty() {
        let title = "── outdated comments";
        let pad = RULER_WIDTH.saturating_sub(title.chars().count());
        push(
            format!("{title}{}", "─".repeat(pad)),
            LineKind::Header,
            None,
        );
        for comment in outdated {
            push_comment_block(&mut push, comment);
        }
    }

    Rendered {
        text,
        line_kinds: line_kinds.into(),
        line_anchors,
    }
}

/// The `── path (status) ──...` separator for one file.
fn file_header(file: &PrFileDiff) -> String {
    let display_path = match (file.old_path == "/dev/null", file.new_path == "/dev/null") {
        (false, false) if file.old_path != file.new_path => {
            format!("{} → {}", file.old_path, file.new_path)
        }
        (_, false) => file.new_path.clone(),
        (false, _) => file.old_path.clone(),
        _ => String::new(),
    };
    let title = format!("── {} ({})", display_path, file.status.as_str());
    let pad = RULER_WIDTH.saturating_sub(title.chars().count());
    format!("{title}{}", "─".repeat(pad))
}

/// Reconstruct the `@@ -a,b +c,d @@ section` line of a hunk.
fn hunk_header(hunk: &PrHunk) -> String {
    let section = if hunk.section.is_empty() {
        String::new()
    } else {
        format!(" {}", hunk.section)
    };
    format!(
        "@@ -{},{} +{},{} @@{section}",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    )
}

/// Render one diff line (prefix + content) and compute its comment anchor.
fn render_diff_line(file: &PrFileDiff, line: &PrDiffLine) -> (String, Option<Anchor>) {
    let prefix = match line.kind {
        DiffLineKind::Add => '+',
        DiffLineKind::Del => '-',
        DiffLineKind::Context => ' ',
    };
    let anchor = match line.kind {
        DiffLineKind::Add => line
            .new_line
            .filter(|_| file.new_path != "/dev/null")
            .map(|line| Anchor {
                path: file.new_path.clone(),
                side: Side::Right,
                line,
            }),
        DiffLineKind::Del => line
            .old_line
            .filter(|_| file.old_path != "/dev/null")
            .map(|line| Anchor {
                path: file.old_path.clone(),
                side: Side::Left,
                line,
            }),
        DiffLineKind::Context => {
            if file.new_path != "/dev/null" {
                line.new_line.map(|line| Anchor {
                    path: file.new_path.clone(),
                    side: Side::Right,
                    line,
                })
            } else {
                line.old_line.map(|line| Anchor {
                    path: file.old_path.clone(),
                    side: Side::Left,
                    line,
                })
            }
        }
    };
    let mut rendered = String::with_capacity(line.text.len() + 1);
    rendered.push(prefix);
    rendered.push_str(&line.text);
    (rendered, anchor)
}

fn kind_of(kind: DiffLineKind) -> LineKind {
    match kind {
        DiffLineKind::Add => LineKind::Add,
        DiffLineKind::Del => LineKind::Del,
        DiffLineKind::Context => LineKind::Context,
    }
}

/// Classify every line of the working-tree files (repo-relative path to one
/// [`LineKind`] per 0-based line of the new file). Only additions carry a
/// highlight; removed lines do not exist in the working tree and show up in
/// the diff gutter instead.
pub fn file_line_kinds(diff: &PrDiff) -> HashMap<String, Arc<[LineKind]>> {
    let mut map: HashMap<String, Arc<[LineKind]>> = HashMap::new();
    for file in &diff.files {
        if file.status == FileStatus::Binary || file.new_path == "/dev/null" {
            continue;
        }
        let mut kinds: Vec<LineKind> = Vec::new();
        for line in file.hunks.iter().flat_map(|hunk| &hunk.lines) {
            if let Some(new_line) = line.new_line {
                let index = new_line as usize - 1;
                if kinds.len() <= index {
                    kinds.resize(index + 1, LineKind::Context);
                }
                kinds[index] = kind_of(line.kind);
            }
        }
        map.insert(file.new_path.clone(), Arc::from(kinds));
    }
    map
}

/// Insert the lines of one comment block below its anchor line.
fn push_comment_block(
    push: &mut impl FnMut(String, LineKind, Option<Anchor>),
    comment: &ReviewComment,
) {
    let login = comment.login();
    let author = if comment.in_reply_to_id.is_some() {
        format!("↳ @{login}:")
    } else {
        format!("@{login}:")
    };
    // "    ┌ " consumes 7 columns.
    const WRAP_WIDTH: usize = 70;
    let mut first = true;
    for line in wrap_text(&comment.body, WRAP_WIDTH) {
        let line = if first {
            format!("    ┌ {author} {line}")
        } else {
            format!("    │ {line}")
        };
        first = false;
        push(line, LineKind::Comment, None);
    }
    push("    └".into(), LineKind::Comment, None);
}

/// Greedy word wrap.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            current.push_str(word);
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_len = word_len;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// A full-line highlight decoration for PR-reviewed buffers.
///
/// Two buffer kinds are decorated:
/// - the scratch review buffer (`:pr-diff`): added, removed, header and
///   comment-block rows are tinted;
/// - real files of the working tree: rows that the PR adds are tinted
///   green. Row backgrounds are derived from the theme's diff foreground
///   colors (`diff.plus`, `diff.minus`, `diff.delta`, `ui.virtual`).
pub fn diff_line_decoration(
    doc_id: DocumentId,
    doc_path: Option<&Path>,
    theme: &Theme,
    inner: Rect,
) -> Option<impl Decoration> {
    // Snapshot the line classifications for this buffer so that rendering
    // never touches the review state.
    let line_kinds = with_review_state(|state| {
        if state.diff_doc == Some(doc_id) {
            return Some(Arc::clone(&state.rendered.line_kinds));
        }
        let path = doc_path?;
        let rel = path.strip_prefix(&state.workspace_root).ok()?.to_str()?;
        state.file_kinds.get(rel).cloned()
    })??;

    fn row_style(theme: &Theme, key: &str) -> Style {
        let mut style = theme.get(key);
        style.bg = style.fg;
        style.fg = None;
        style
    }
    let style_add = row_style(theme, "diff.plus");
    let style_del = row_style(theme, "diff.minus");
    let style_header = row_style(theme, "diff.delta");
    let style_comment = row_style(theme, "ui.virtual");

    Some(move |renderer: &mut TextRenderer, pos: LinePos| {
        let Some(&kind) = line_kinds.get(pos.doc_line) else {
            return;
        };
        let style = match kind {
            LineKind::Add => style_add,
            LineKind::Del => style_del,
            LineKind::Header => style_header,
            LineKind::Comment => style_comment,
            LineKind::Context => return,
        };
        renderer.set_style(Rect::new(inner.x, pos.visual_line, inner.width, 1), style);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::diff::parse_unified_diff;
    use crate::review::gh::GhUser;

    fn detail() -> PrDetail {
        PrDetail {
            number: 3,
            title: "Fix the thing".into(),
            author: GhUser {
                login: "alice".into(),
            },
            base_ref_name: "main".into(),
            head_ref_name: "fix-thing".into(),
            head_ref_oid: "0123456789abcdef".into(),
            base_ref_oid: "fedcba9876543210".into(),
            url: "https://github.com/owner/repo/pull/3".into(),
        }
    }

    fn comment(
        id: u64,
        login: &str,
        path: &str,
        side: Side,
        line: u32,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            body: body.into(),
            path: Some(path.into()),
            line: Some(line),
            original_line: Some(line),
            side: Some(side.as_str().into()),
            in_reply_to_id: None,
            user: Some(GhUser {
                login: login.into(),
            }),
            created_at: String::new(),
        }
    }

    const SIMPLE_DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,3 +10,4 @@ fn parse() {
+    let added = 1;
     let context = 2;
-    let removed = 3;
 }
";

    #[test]
    fn renders_diff_and_anchors() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let rendered = render_review(&detail(), &diff, &[]);

        assert_eq!(rendered.line_kinds.len(), rendered.line_anchors.len());
        assert!(rendered.text.starts_with("PR #3: Fix the thing\n"));

        // 4 headers + 1 file header + 1 hunk header + 4 diff lines
        let diff_lines = rendered
            .line_kinds
            .iter()
            .filter(|kind| matches!(kind, LineKind::Add | LineKind::Del | LineKind::Context))
            .count();
        assert_eq!(diff_lines, 4);

        // The first RIGHT-side anchor belongs to the `+` line (new line 10).
        let (anchor_line, anchor) = rendered
            .line_anchors
            .iter()
            .enumerate()
            .find(|(_, anchor)| {
                anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.side == Side::Right)
            })
            .unwrap();
        assert_eq!(anchor.as_ref().unwrap().line, 10);
        assert_eq!(anchor.as_ref().unwrap().path, "src/lib.rs");
        assert!(rendered
            .text
            .lines()
            .nth(anchor_line)
            .unwrap()
            .starts_with('+'));
    }

    #[test]
    fn inserts_comment_blocks() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let comments = vec![
            comment(1, "alice", "src/lib.rs", Side::Right, 10, "why 1?"),
            comment(2, "bob", "src/lib.rs", Side::Left, 11, "keep 3"),
        ];
        let rendered = render_review(&detail(), &diff, &comments);

        let text = &rendered.text;
        assert!(text.contains("┌ @alice: why 1?"));
        assert!(text.contains("┌ @bob: keep 3"));
        assert!(text.contains("└"));
        // Comment blocks are inserted after their anchor line, before the
        // following diff line.
        let plus = text.find("+    let added = 1;").unwrap();
        let alice = text.find("┌ @alice").unwrap();
        let minus = text.find("-    let removed = 3;").unwrap();
        let bob = text.find("┌ @bob").unwrap();
        let context = text.find("     let context = 2;").unwrap();
        assert!(
            plus < alice && alice < context,
            "alice block must follow the + line"
        );
        assert!(
            context < minus && minus < bob,
            "bob block must follow the - line"
        );
        // No "outdated comments" section should be emitted.
        assert!(!text.contains("outdated comments"));
        // Every comment line is marked as a Comment kind.
        assert!(rendered
            .line_kinds
            .iter()
            .any(|kind| matches!(kind, LineKind::Comment)));
    }

    #[test]
    fn collects_outdated_comments() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let comments = vec![comment(1, "alice", "src/lib.rs", Side::Right, 999, "stale")];
        let rendered = render_review(&detail(), &diff, &comments);
        assert!(rendered.text.contains("outdated comments"));
        assert!(rendered.text.contains("┌ @alice: stale"));
    }

    #[test]
    fn wraps_long_comment_bodies() {
        let lines = wrap_text("one two three four five six seven eight nine ten", 10);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| line.chars().count() <= 10));
    }

    #[test]
    fn empty_diff_renders_header_only() {
        let diff = PrDiff::default();
        let rendered = render_review(&detail(), &diff, &[]);
        assert!(rendered.text.starts_with("PR #3: Fix the thing\n"));
        assert_eq!(rendered.line_kinds.len(), 4);
    }

    #[test]
    fn classifies_real_file_lines() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let kinds = file_line_kinds(&diff);
        let kinds = kinds.get("src/lib.rs").unwrap();
        // The fixture's `+    let added = 1;` is the only added line; all
        // other lines are context or absent from the working tree.
        assert_eq!(kinds[10 - 1], LineKind::Add); // new_line 10
        assert_eq!(kinds[11 - 1], LineKind::Context);
        assert_eq!(kinds[12 - 1], LineKind::Context);
    }

    #[test]
    fn file_line_kinds_skips_binary_and_deleted_files() {
        let input = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hi
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-byebye
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
";
        let diff = parse_unified_diff(input);
        let kinds = file_line_kinds(&diff);
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds["new.txt"][0], LineKind::Add);
    }
}
