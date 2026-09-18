//! Generation of the review (diff) buffer and its full-line highlights.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use helix_core::text_annotations::LineAnnotation;
use helix_core::Position;
use helix_view::graphics::{Color, Modifier, Rect};
use helix_view::theme::{Style, Theme};
use helix_view::DocumentId;

use crate::ui::{Decoration, LinePos, TextRenderer};

use super::diff::{DiffLineKind, FileStatus, PrDiff, PrDiffLine, PrFileDiff, PrHunk};
use super::gh::{PrDetail, ReviewComment, Side};
use super::{
    with_review_state, Anchor, LineKind, PendingComment, Rendered, RowSpan, RowSpanKind,
    VirtualBlock, VirtualRow, VirtualRowKind,
};

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
pub fn render_review(
    detail: &PrDetail,
    diff: &PrDiff,
    comments: &[ReviewComment],
    pending: &[PendingComment],
) -> Rendered {
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
    let mut pending_map: HashMap<(String, Side, u32), Vec<usize>> = HashMap::new();
    for (index, comment) in pending.iter().enumerate() {
        pending_map
            .entry((
                comment.anchor.path.clone(),
                comment.anchor.side,
                comment.anchor.line,
            ))
            .or_default()
            .push(index);
    }
    let mut pending_placed: HashSet<usize> = HashSet::new();

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
                            push_comment_block(
                                &mut push,
                                &author_prefix(&comments[index]),
                                comment_span(
                                    comment_range(&comments[index]),
                                    comments[index].line.or(comments[index].original_line),
                                ),
                                &comments[index].body,
                            );
                        }
                    }
                    if let Some(indices) = pending_map.get(&key) {
                        for &index in indices {
                            pending_placed.insert(index);
                            push_comment_block(
                                &mut push,
                                "you (pending):",
                                comment_span(
                                    pending[index].anchor.start_line,
                                    Some(pending[index].anchor.line),
                                ),
                                &pending[index].body,
                            );
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
            push_comment_block(
                &mut push,
                &author_prefix(comment),
                comment_span(
                    comment_range(comment),
                    comment.line.or(comment.original_line),
                ),
                &comment.body,
            );
        }
    }

    // Pending comments whose anchor did not match any diff line.
    let unplaced: Vec<&PendingComment> = pending
        .iter()
        .enumerate()
        .filter(|(index, _)| !pending_placed.contains(index))
        .map(|(_, comment)| comment)
        .collect();
    if !unplaced.is_empty() {
        let title = "── pending comments";
        let pad = RULER_WIDTH.saturating_sub(title.chars().count());
        push(
            format!("{title}{}", "─".repeat(pad)),
            LineKind::Header,
            None,
        );
        for comment in unplaced {
            push_comment_block(
                &mut push,
                "you (pending):",
                comment_span(comment.anchor.start_line, Some(comment.anchor.line)),
                &comment.body,
            );
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
                start_line: None,
            }),
        DiffLineKind::Del => line
            .old_line
            .filter(|_| file.old_path != "/dev/null")
            .map(|line| Anchor {
                path: file.old_path.clone(),
                side: Side::Left,
                line,
                start_line: None,
            }),
        DiffLineKind::Context => {
            if file.new_path != "/dev/null" {
                line.new_line.map(|line| Anchor {
                    path: file.new_path.clone(),
                    side: Side::Right,
                    line,
                    start_line: None,
                })
            } else {
                line.old_line.map(|line| Anchor {
                    path: file.old_path.clone(),
                    side: Side::Left,
                    line,
                    start_line: None,
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

/// Lines of each reviewed file that may receive a review comment (1-based
/// new-file line numbers that are part of the diff: added and context lines).
pub fn commentable_lines(diff: &PrDiff) -> HashMap<String, BTreeSet<u32>> {
    let mut map: HashMap<String, BTreeSet<u32>> = HashMap::new();
    for file in &diff.files {
        if file.status == FileStatus::Binary || file.new_path == "/dev/null" {
            continue;
        }
        let lines: Vec<u32> = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter_map(|line| line.new_line)
            .collect();
        if !lines.is_empty() {
            map.insert(file.new_path.clone(), lines.into_iter().collect());
        }
    }
    map
}

/// Virtual rows for a working-tree file: the PR's deleted lines (shown for
/// reference without touching the file) and the comment blocks (remote and
/// pending) anchored to its lines. Blocks are ordered by anchor line.
pub fn file_virtual_blocks(
    file: &PrFileDiff,
    comments: &[ReviewComment],
    pending: &[PendingComment],
    width: usize,
) -> Vec<VirtualBlock> {
    let mut blocks: Vec<VirtualBlock> = Vec::new();

    // Deleted lines are anchored below the surviving line that precedes the
    // deletion run (or below the first line for deletions at the file top).
    for hunk in &file.hunks {
        let mut run: Vec<VirtualRow> = Vec::new();
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Del => run.push(VirtualRow {
                    kind: VirtualRowKind::Deleted,
                    spans: vec![RowSpan {
                        text: format!("-{}", line.text),
                        kind: RowSpanKind::Content,
                    }],
                }),
                DiffLineKind::Add | DiffLineKind::Context => {
                    if !run.is_empty() {
                        if let Some(survivor) = line.new_line {
                            let anchor = (survivor as usize - 1).saturating_sub(1);
                            push_block(&mut blocks, anchor, &run);
                        }
                        run.clear();
                    }
                }
            }
        }
        if !run.is_empty() {
            if let Some(survivor) = hunk.lines.iter().rev().find_map(|line| line.new_line) {
                let anchor = (survivor as usize - 1).saturating_sub(1);
                push_block(&mut blocks, anchor, &run);
            }
        }
    }

    // Comments (remote then pending) anchored on the new side of the diff.
    let mut comment_blocks: Vec<VirtualBlock> = Vec::new();
    for comment in comments {
        let Some((path, side, line)) = comment.anchor() else {
            continue;
        };
        if side != Side::Right || path != file.new_path {
            continue;
        }
        push_comment_rows(
            &mut comment_blocks,
            line,
            &comment.body,
            &format_author(
                &author_prefix(comment),
                comment_span(comment_range(comment), Some(line)),
            ),
            VirtualRowKind::Comment,
            width,
        );
    }
    for comment in pending {
        if comment.anchor.side != Side::Right || comment.anchor.path != file.new_path {
            continue;
        }
        push_comment_rows(
            &mut comment_blocks,
            comment.anchor.line,
            &comment.body,
            &format_author(
                "you (pending):",
                comment_span(comment.anchor.start_line, Some(comment.anchor.line)),
            ),
            VirtualRowKind::Pending,
            width,
        );
    }

    blocks.append(&mut comment_blocks);
    blocks.sort_by_key(|block| block.line);
    blocks
}

fn push_comment_rows(
    blocks: &mut Vec<VirtualBlock>,
    anchor_line: u32,
    body: &str,
    author: &str,
    kind: VirtualRowKind,
    width: usize,
) {
    let mut rows: Vec<VirtualRow> = Vec::new();
    let body_width = width.saturating_sub(6).max(10);
    let mut first = true;
    for wrapped in wrap_text(body, body_width) {
        rows.push(VirtualRow {
            kind,
            spans: if first {
                vec![
                    RowSpan {
                        text: "    ┌ ".into(),
                        kind: RowSpanKind::Frame,
                    },
                    RowSpan {
                        text: author.into(),
                        kind: RowSpanKind::Author,
                    },
                    RowSpan {
                        text: format!(" {wrapped}"),
                        kind: RowSpanKind::Body,
                    },
                ]
            } else {
                vec![
                    RowSpan {
                        text: "    │ ".into(),
                        kind: RowSpanKind::Frame,
                    },
                    RowSpan {
                        text: wrapped,
                        kind: RowSpanKind::Body,
                    },
                ]
            },
        });
        first = false;
    }
    rows.push(VirtualRow {
        kind,
        spans: vec![RowSpan {
            text: "    └".into(),
            kind: RowSpanKind::Frame,
        }],
    });
    push_block(blocks, anchor_line.saturating_sub(1) as usize, &rows);
}

fn push_block(blocks: &mut Vec<VirtualBlock>, line: usize, rows: &[VirtualRow]) {
    blocks.push(VirtualBlock {
        line,
        rows: rows.to_vec(),
    });
}

/// Insert the lines of one comment block below its anchor line.
fn push_comment_block(
    push: &mut impl FnMut(String, LineKind, Option<Anchor>),
    author: &str,
    span: Option<(u32, u32)>,
    body: &str,
) {
    // "    ┌ " consumes 7 columns.
    const WRAP_WIDTH: usize = 70;
    let author = format_author(author, span);
    let mut first = true;
    for line in wrap_text(body, WRAP_WIDTH) {
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

/// The first line of a multi-line comment, if it spans a range.
fn comment_range(comment: &ReviewComment) -> Option<u32> {
    comment.start_line.or(comment.original_start_line)
}

/// The (start, end) span of a multi-line comment, or `None` for single-line
/// comments.
fn comment_span(start_line: Option<u32>, end_line: Option<u32>) -> Option<(u32, u32)> {
    Some((start_line?, end_line?))
}

/// `@alice (lines 12-15):` when the comment spans several lines.
fn format_author(author: &str, span: Option<(u32, u32)>) -> String {
    match span {
        Some((start, end)) => format!("{} (lines {start}-{end}):", author.trim_end_matches(':')),
        None => author.to_string(),
    }
}

/// `@login:` or `↳ @login:` for replies.
fn author_prefix(comment: &ReviewComment) -> String {
    if comment.in_reply_to_id.is_some() {
        format!("↳ @{}:", comment.login())
    } else {
        format!("@{}:", comment.login())
    }
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

/// The diff highlight colors are designed as foregrounds; used as row
/// backgrounds they are glaring, so they are blended toward black.
fn dimmed(color: Color) -> Color {
    const FACTOR: f32 = 0.4;
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * FACTOR) as u8,
            (g as f32 * FACTOR) as u8,
            (b as f32 * FACTOR) as u8,
        ),
        other => other,
    }
}

/// A row background derived from a diff theme color: the theme's foreground
/// is reused as the background, dimmed.
fn diff_row_background(theme: &Theme, key: &str) -> Style {
    let mut style = theme.get(key);
    style.bg = style.fg.map(dimmed);
    style.fg = None;
    style
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

    let style_add = diff_row_background(theme, "diff.plus");
    let style_del = diff_row_background(theme, "diff.minus");
    let style_header = diff_row_background(theme, "diff.delta");
    let style_comment = diff_row_background(theme, "ui.virtual");

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

/// Virtual rows shown inside a working-tree file: deleted lines and comment
/// blocks. Implements both halves of the virtual-line machinery:
///  - [`LineAnnotation`] reserves the rows (space) below the anchor line;
///  - [`Decoration`] draws the row content.
#[derive(Clone)]
pub struct ReviewVirtualLines {
    /// (anchor doc line, rows, char index of the anchor line end) sorted by
    /// anchor line. The line-end char distinguishes the last visual line of
    /// a soft-wrapped anchor line from its earlier segments.
    entries: Vec<(usize, Vec<VirtualRow>, usize)>,
    /// Background of a deleted-line row.
    style_row_deleted: Style,
    /// Background of a comment row.
    style_row_comment: Style,
    /// Background of a pending-comment row.
    style_row_pending: Style,
    /// Usernames are highlighted; the comment text itself stays muted.
    style_author: Style,
    style_body: Style,
    style_frame: Style,
}

/// Style for comment authors: themes may define `review.comment.author`, and
/// otherwise the `warning` scope (usually a yellow) is used.
fn author_style(theme: &Theme) -> Style {
    theme
        .try_get("review.comment.author")
        .or_else(|| theme.try_get("warning"))
        .unwrap_or_else(|| Style::default().fg(helix_view::graphics::Color::Yellow))
}

/// Style for the comment text: themes may define `review.comment.body`, with
/// the `comment` syntax scope (usually a muted gray) as the fallback.
fn body_style(theme: &Theme) -> Style {
    theme
        .try_get("review.comment.body")
        .or_else(|| theme.try_get("comment"))
        .unwrap_or_else(|| Style::default().fg(helix_view::graphics::Color::Gray))
}

/// Builders used by the comment block styles.
trait StyleExt {
    /// Same colors, dimmed.
    fn with_dim(self) -> Style;
}

impl StyleExt for Style {
    fn with_dim(mut self) -> Style {
        self.sub_modifier |= Modifier::DIM;
        self
    }
}

impl ReviewVirtualLines {
    /// Build the annotation/decoration for a document. `line_end_char` of
    /// each anchor line is derived from `doc`'s text.
    pub fn new(doc: &helix_view::Document, blocks: Vec<VirtualBlock>, theme: &Theme) -> Self {
        let text = doc.text();
        let mut entries: Vec<(usize, Vec<VirtualRow>, usize)> = blocks
            .into_iter()
            .map(|block| {
                let line_end = text.line_to_char(block.line + 1).saturating_sub(1);
                (block.line, block.rows, line_end)
            })
            .collect();
        entries.sort_by_key(|(line, _, _)| *line);

        let style_body = body_style(theme);
        ReviewVirtualLines {
            entries,
            style_row_deleted: diff_row_background(theme, "diff.minus"),
            style_row_comment: diff_row_background(theme, "ui.virtual"),
            style_row_pending: diff_row_background(theme, "diff.delta"),
            style_author: author_style(theme),
            style_body,
            style_frame: style_body.with_dim(),
        }
    }
}

/// Build the virtual-line annotation + decoration pair for a document, if it
/// is a reviewed file with anything to show (deleted lines or comments).
/// Both halves share the same blocks so the reserved rows and the drawn rows
/// always match.
pub fn review_virtual_lines(
    doc: &helix_view::Document,
    theme: &Theme,
    width: usize,
) -> Option<(ReviewVirtualLines, ReviewVirtualLines)> {
    let path = doc.path()?;
    let blocks: Option<Vec<VirtualBlock>> = with_review_state(|state| {
        let rel = path.strip_prefix(&state.workspace_root).ok()?.to_str()?;
        let file = state.diff.files.iter().find(|file| file.new_path == rel)?;
        Some(file_virtual_blocks(
            file,
            &state.comments,
            &state.pending,
            width,
        ))
    })
    .flatten()
    .filter(|blocks| !blocks.is_empty());
    let blocks = blocks?;
    let annotation = ReviewVirtualLines::new(doc, blocks.clone(), theme);
    let decoration = ReviewVirtualLines::new(doc, blocks, theme);
    Some((annotation, decoration))
}

impl LineAnnotation for ReviewVirtualLines {
    fn reset_pos(&mut self, _char_idx: usize) -> usize {
        usize::MAX
    }

    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        let rows = self
            .entries
            .iter()
            .find(|(line, _, line_end)| *line == doc_line && line_end_char_idx >= *line_end)
            .map(|(_, rows, _)| rows.len())
            .unwrap_or(0);
        Position::new(rows, 0)
    }
}

impl ReviewVirtualLines {
    /// The background style of a row, chosen by what the row shows.
    fn row_style(&self, kind: VirtualRowKind) -> Style {
        match kind {
            VirtualRowKind::Deleted => self.style_row_deleted,
            VirtualRowKind::Comment => self.style_row_comment,
            VirtualRowKind::Pending => self.style_row_pending,
        }
    }

    /// Foreground style of a span, patched onto the row's background so the
    /// row stays visually contiguous.
    fn span_style(&self, kind: RowSpanKind, row: Style) -> Style {
        match kind {
            RowSpanKind::Author => row.patch(self.style_author),
            RowSpanKind::Body => row.patch(self.style_body),
            RowSpanKind::Frame => row.patch(self.style_frame),
            RowSpanKind::Content => row,
        }
    }
}

impl Decoration for ReviewVirtualLines {
    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some((_, rows, _)) = self
            .entries
            .iter()
            .find(|(line, _, _)| *line == pos.doc_line)
        else {
            return Position::new(0, 0);
        };
        let start_row = pos.visual_line + virt_off.row as u16;
        // Rows below the viewport bottom would panic the renderer; they are
        // simply not drawn (the reservation stays, matching the layout).
        let viewport_bottom = renderer.viewport.y + renderer.viewport.height;
        for (i, row) in rows.iter().enumerate() {
            let y = start_row + i as u16;
            if y >= viewport_bottom {
                break;
            }
            let style = self.row_style(row.kind);
            // Paint the full row so the block reads as a contiguous area,
            // then draw the styled spans on top of it.
            renderer.set_style(
                Rect::new(renderer.viewport.x, y, renderer.viewport.width, 1),
                style,
            );
            let row_end = renderer.viewport.x + renderer.viewport.width;
            let mut x = renderer.viewport.x;
            for span in &row.spans {
                if x >= row_end {
                    break;
                }
                let span_style = self.span_style(span.kind, style);
                let width = (row_end - x) as usize;
                let (next_x, _) = renderer.set_string_truncated(
                    x,
                    y,
                    &span.text,
                    width,
                    |_| span_style,
                    false,
                    false,
                );
                x = next_x;
            }
        }
        Position::new(rows.len(), 0)
    }
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
            start_line: None,
            original_line: Some(line),
            original_start_line: None,
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
        let rendered = render_review(&detail(), &diff, &[], &[]);

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
        let rendered = render_review(&detail(), &diff, &comments, &[]);

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
        let rendered = render_review(&detail(), &diff, &comments, &[]);
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
    fn renders_multi_line_pending_blocks() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let pending = vec![PendingComment {
            anchor: Anchor {
                path: "src/lib.rs".into(),
                side: Side::Right,
                line: 12,
                start_line: Some(10),
            },
            body: "this block".into(),
        }];
        let rendered = render_review(&detail(), &diff, &[], &pending);
        assert!(
            rendered
                .text
                .contains("┌ you (pending) (lines 10-12): this block"),
            "multi-line pending comment should announce its range: {}\n{}",
            rendered.text,
            rendered.text
        );
    }

    #[test]
    fn renders_pending_comment_blocks() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let pending = vec![PendingComment {
            anchor: Anchor {
                path: "src/lib.rs".into(),
                side: Side::Right,
                line: 10,
                start_line: None,
            },
            body: "please rename".into(),
        }];
        let rendered = render_review(&detail(), &diff, &[], &pending);
        assert!(rendered.text.contains("┌ you (pending): please rename"));
        assert!(!rendered.text.contains("outdated comments"));
        assert!(!rendered.text.contains("── pending comments"));
    }

    #[test]
    fn empty_diff_renders_header_only() {
        let diff = PrDiff::default();
        let rendered = render_review(&detail(), &diff, &[], &[]);
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

    #[test]
    fn row_backgrounds_are_dimmed() {
        let theme = helix_view::theme::Loader::new(&[]).default_theme(true);
        let background = diff_row_background(&theme, "diff.plus");
        let foreground = theme.get("diff.plus").fg;
        match (background.bg, foreground) {
            (Some(Color::Rgb(br, bg_, bb)), Some(Color::Rgb(fr, fg_, fb))) => {
                assert!(br < fr && bg_ < fg_ && bb < fb, "background must be darker");
            }
            _ => panic!("expected rgb colors"),
        }
    }

    #[test]
    fn comment_styles_resolve_to_colors() {
        // The default theme must produce visible colors for both parts of a
        // comment, otherwise the block would render unstyled.
        let theme = helix_view::theme::Loader::new(&[]).default_theme(true);
        let author = author_style(&theme);
        let body = body_style(&theme);
        assert!(
            author.fg.is_some(),
            "comment authors should resolve to a color"
        );
        assert!(body.fg.is_some(), "comment text should resolve to a color");
        assert_ne!(
            author.fg, body.fg,
            "authors and comment text should be distinguishable"
        );
    }

    #[test]
    fn builds_file_virtual_blocks() {
        let diff = parse_unified_diff(SIMPLE_DIFF);
        let file = diff
            .files
            .iter()
            .find(|file| file.new_path == "src/lib.rs")
            .unwrap();
        let comments = vec![comment(1, "alice", "src/lib.rs", Side::Right, 12, "nice")];
        let pending = vec![PendingComment {
            anchor: Anchor {
                path: "src/lib.rs".into(),
                side: Side::Right,
                line: 10,
                start_line: None,
            },
            body: "rename".into(),
        }];
        let blocks = file_virtual_blocks(file, &comments, &pending, 120);

        // SIMPLE_DIFF: `-    let removed = 3;` (old_line 11) is followed by
        // the context line `}` (new_line 12), so the deleted line is anchored
        // below the line before it, i.e. index 10 (new line 11).
        let deleted = blocks
            .iter()
            .find(|block| {
                block
                    .rows
                    .iter()
                    .any(|row| row.kind == VirtualRowKind::Deleted)
            })
            .unwrap();
        assert_eq!(deleted.line, 10);
        assert_eq!(deleted.rows[0].text(), "-    let removed = 3;");

        // The remote comment (new line 12) and the pending comment (new line
        // 10) get blocks below their anchor lines, with the pending one
        // clearly marked.
        let comment = blocks
            .iter()
            .find(|block| {
                block
                    .rows
                    .iter()
                    .any(|row| row.kind == VirtualRowKind::Comment)
            })
            .unwrap();
        assert_eq!(comment.line, 11);
        assert!(comment.rows[0].text().contains("┌ @alice: nice"));
        // The username and the comment text are separate spans so they can be
        // colored differently.
        let spans = &comment.rows[0].spans;
        assert_eq!(spans[1].kind, RowSpanKind::Author);
        assert_eq!(spans[1].text, "@alice:");
        assert_eq!(spans[2].kind, RowSpanKind::Body);
        assert_eq!(spans[2].text, " nice");
        assert_eq!(spans[0].kind, RowSpanKind::Frame);
        let pending_block = blocks
            .iter()
            .find(|block| {
                block
                    .rows
                    .iter()
                    .any(|row| row.kind == VirtualRowKind::Pending)
            })
            .unwrap();
        assert_eq!(pending_block.line, 9);
        assert!(pending_block.rows[0]
            .text()
            .contains("┌ you (pending): rename"));
        // Blocks are sorted by anchor line.
        let lines: Vec<usize> = blocks.iter().map(|block| block.line).collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted);
    }
}
