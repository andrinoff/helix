//! State of the currently loaded pull request review session.
//!
//! The state is owned by a main-thread global rather than by the [`Editor`]
//! so that helix-view does not need to know anything about GitHub. It is
//! written from command / async job callbacks and read by the renderer and
//! the pickers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use helix_view::DocumentId;

use crate::job;

#[cfg(unix)]
pub mod askpass;
pub mod diff;
pub mod gh;
pub mod render;

pub use diff::{DiffLineKind, FileStatus, PrDiff, PrDiffLine, PrFileDiff, PrHunk};
pub use gh::{PrDetail, ReviewComment, Side};
pub use render::{
    commentable_lines, diff_line_decoration, file_line_kinds, render_review, review_virtual_lines,
};

fn review_state_cell() -> &'static Mutex<Option<ReviewState>> {
    static REVIEW_STATE: OnceLock<Mutex<Option<ReviewState>>> = OnceLock::new();
    REVIEW_STATE.get_or_init(|| Mutex::new(None))
}

/// Run `f` with mutable access to the current review state, if one is
/// loaded. Returns `None` otherwise. `f` must not re-enter the state.
pub fn with_review_state<R>(f: impl FnOnce(&mut ReviewState) -> R) -> Option<R> {
    let mut guard = review_state_cell().lock().ok()?;
    if guard.is_none() {
        return None;
    }
    Some(f(guard.as_mut().unwrap()))
}

/// Replace (or clear, with `None`) the current review state.
pub fn set_review_state(state: Option<ReviewState>) {
    *review_state_cell().lock().unwrap() = state;
}

/// Classification of a rendered line in the review buffer. The renderer uses
/// this to pick a full-line highlight style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// PR / file / hunk headers and separators.
    Header,
    /// `+` added line.
    Add,
    /// `-` removed line.
    Del,
    /// Unchanged context line.
    Context,
    /// An inserted comment block line.
    Comment,
}

/// Maps a rendered line of the review buffer back to a location in the PR
/// diff that a comment can be anchored to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// Repository-relative path, exactly as reported by the diff and the
    /// comments API.
    pub path: String,
    pub side: Side,
    /// 1-based line number on the given side.
    pub line: u32,
}

/// The review buffer contents together with per-line metadata. The line
/// classifications are shared (cheaply cloneable) so decorations can snapshot
/// them without locking the review state during rendering.
#[derive(Debug)]
pub struct Rendered {
    pub text: String,
    /// Classification of each line, parallel to `text`.
    pub line_kinds: Arc<[LineKind]>,
    /// Comment anchor of each line, parallel to `text`.
    pub line_anchors: Vec<Option<Anchor>>,
}

/// A comment that has not been submitted yet. Pending comments stay local
/// (visible in the files, the diff buffer and the comment picker) until a
/// review is published with `:review`.
#[derive(Debug, Clone)]
pub struct PendingComment {
    pub anchor: Anchor,
    pub body: String,
}

/// One styled run of text inside a [`VirtualRow`].
#[derive(Debug, Clone)]
pub struct RowSpan {
    pub text: String,
    pub kind: RowSpanKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSpanKind {
    /// Box-drawing frame of a comment block (`┌ │ └`).
    Frame,
    /// The commenter's name.
    Author,
    /// The text of a comment.
    Body,
    /// Content of a deleted line.
    Content,
}

/// One rendered row of a virtual block (deleted lines / comment blocks shown
/// inside a real file without touching its content).
#[derive(Debug, Clone)]
pub struct VirtualRow {
    pub kind: VirtualRowKind,
    pub spans: Vec<RowSpan>,
}

impl VirtualRow {
    /// The row's text, without styling.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualRowKind {
    /// A line removed by the PR, shown for reference only.
    Deleted,
    /// A comment block (review comment).
    Comment,
    /// A comment that has not been submitted yet.
    Pending,
}

/// Virtual rows anchored below a document line.
#[derive(Debug, Clone)]
pub struct VirtualBlock {
    /// 0-based document line the virtual rows are anchored below.
    pub line: usize,
    pub rows: Vec<VirtualRow>,
}

/// Everything helix knows about the PR currently being reviewed.
pub struct ReviewState {
    /// Workspace root the PR was checked out in.
    pub workspace_root: PathBuf,
    pub detail: PrDetail,
    pub owner: String,
    pub repo: String,
    pub diff: PrDiff,
    pub comments: Vec<ReviewComment>,
    pub rendered: Rendered,
    /// Document the review buffer is displayed in, if it is still open.
    pub diff_doc: Option<DocumentId>,
    /// Commit the PR files are diffed against (the merge base of the head
    /// and base branches).
    pub diff_base_sha: String,
    /// Per-file line classifications for the real files (repo-relative path
    /// to a `LineKind` per 0-based line of the working-tree file), used to
    /// highlight added lines in place.
    pub file_kinds: HashMap<String, Arc<[LineKind]>>,
    /// Lines of each reviewed file that may receive a review comment
    /// (1-based new-file line numbers that are part of the diff).
    pub commentable: HashMap<String, std::collections::BTreeSet<u32>>,
    /// Comments written locally, pending review submission.
    pub pending: Vec<PendingComment>,
}

/// Fetch the base-branch content of `rel` and install it as the diff base of
/// the given document. Used whenever a file of a reviewed PR is opened so
/// the working tree shows the PR's changes in place.
pub fn schedule_base_fetch(
    cwd: PathBuf,
    owner: String,
    repo: String,
    base_sha: String,
    rel: String,
    doc_id: DocumentId,
) {
    tokio::spawn(async move {
        let Ok(content) = gh::fetch_base_content(&cwd, &owner, &repo, &rel, &base_sha).await else {
            return;
        };
        job::dispatch_blocking(move |editor, _| {
            if let Some(doc) = editor.document_mut(doc_id) {
                doc.set_diff_base(content);
            }
        });
    });
}
