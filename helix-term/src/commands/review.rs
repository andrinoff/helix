//! `:pr*` commands for GitHub pull request review.
//!
//! Flow: `:pr` lists open PRs, checking one out loads the PR diff and its
//! review comments into a read-only diff buffer with inline comment blocks.
//! `:pr-comment` posts a comment for the diff line under the cursor.

use std::path::{Path, PathBuf};

use helix_core::command_line::Args;
use helix_core::Rope;
use helix_view::editor::Action;
use helix_view::{DocumentId, Editor, ViewId};

use crate::compositor::{self, Compositor};
use crate::job::{self, Callback};
use crate::review::diff::{parse_unified_diff, FileStatus};
use crate::review::gh::{self, PrDetail, PrListItem, Side};
use crate::review::{self, file_line_kinds, render_review, ReviewState};
use crate::ui::overlay::overlaid;
use crate::ui::{self, Picker, PickerColumn, Prompt, PromptEvent};

pub(crate) fn pr(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }

    if let Some(number) = args.first().and_then(|arg| arg.parse::<u64>().ok()) {
        start_review(cx, workspace_root(cx), number);
        return Ok(());
    }
    if args.first().is_some() {
        cx.editor
            .set_error("Invalid pull request number; expected :pr <number>");
        return Ok(());
    }
    pr_list_picker(cx);
    Ok(())
}

pub(crate) fn pr_diff(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    if let Err(error) = open_diff_buffer(cx.editor) {
        cx.editor.set_error(error.to_string());
    }
    Ok(())
}

pub(crate) fn pr_files(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let Some((_root, files)) = review_file_entries() else {
        cx.editor
            .set_error("No pull request is loaded; run :pr first");
        return Ok(());
    };

    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                push_files_picker(compositor, files);
            },
        )))
    });
    Ok(())
}

/// The openable files of the loaded PR (with `root` for joining), read from
/// the review state.
fn review_file_entries() -> Option<(PathBuf, Vec<PrFileEntry>)> {
    review::with_review_state(|state| {
        let root = state.workspace_root.clone();
        let files = state
            .diff
            .files
            .iter()
            .map(|file| PrFileEntry {
                display: file.new_path.clone(),
                path: (file.status != FileStatus::Removed && file.new_path != "/dev/null")
                    .then(|| root.join(&file.new_path)),
                status: file.status,
            })
            .collect();
        (root, files)
    })
}

/// Push the changed-files picker. Must run inside a compositor callback.
fn push_files_picker(compositor: &mut Compositor, files: Vec<PrFileEntry>) {
    let columns = [
        PickerColumn::new("file", |entry: &PrFileEntry, _data: &()| {
            entry.display.clone().into()
        }),
        PickerColumn::new("status", |entry: &PrFileEntry, _data: &()| {
            entry.status.as_str().into()
        }),
    ];
    let picker = Picker::new(columns, 0, files, (), |cx, entry: &PrFileEntry, action| {
        let Some(path) = &entry.path else {
            cx.editor
                .set_error("This file was removed by the pull request");
            return;
        };
        if let Err(error) = cx.editor.open(path, action) {
            cx.editor.set_error(format!("Could not open file: {error}"));
        }
    })
    .with_preview(|_editor, entry| {
        entry
            .path
            .as_ref()
            .map(|path| (path.as_path().into(), None))
    });
    compositor.push(Box::new(overlaid(picker)));
}

pub(crate) fn pr_comments(
    cx: &mut compositor::Context,
    _args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let entries = review::with_review_state(|state| {
        let root = state.workspace_root.clone();
        let mut entries: Vec<CommentEntry> = Vec::new();
        for comment in &state.comments {
            let (path, side, line) = comment
                .anchor()
                .map(|(path, side, line)| (path.to_string(), side, line))
                .unwrap_or((String::new(), Side::Right, 0));
            entries.push(CommentEntry {
                author: format!("@{}", comment.login()),
                path: root.join(&path),
                location: format!(
                    "{}:{} ({})",
                    path,
                    line,
                    match side {
                        Side::Right => "new",
                        Side::Left => "old",
                    }
                ),
                body: comment.body.clone(),
                line: line as usize,
            });
        }
        for comment in &state.pending {
            let anchor = &comment.anchor;
            let side = anchor.side;
            entries.push(CommentEntry {
                author: "you (pending)".into(),
                path: root.join(&anchor.path),
                location: format!(
                    "{}:{} ({})",
                    anchor.path,
                    anchor.line,
                    match side {
                        Side::Right => "new",
                        Side::Left => "old",
                    }
                ),
                body: comment.body.clone(),
                line: anchor.line as usize,
            });
        }
        entries
    });
    let Some(entries) = entries else {
        cx.editor
            .set_error("No pull request is loaded; run :pr first");
        return Ok(());
    };

    let columns = [
        PickerColumn::new("author", |entry: &CommentEntry, _data: &()| {
            entry.author.clone().into()
        }),
        PickerColumn::new("location", |entry: &CommentEntry, _data: &()| {
            entry.location.clone().into()
        }),
        PickerColumn::new("comment", |entry: &CommentEntry, _data: &()| {
            one_line(&entry.body).into()
        }),
    ];
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let picker = Picker::new(
                    columns,
                    2,
                    entries,
                    (),
                    |cx, entry: &CommentEntry, action| {
                        let Some(path) = entry.existing_path() else {
                            cx.editor
                                .set_error("The commented file does not exist in the working tree");
                            return;
                        };
                        let Ok(doc_id) = cx.editor.open(path, action) else {
                            cx.editor.set_error("Could not open the commented file");
                            return;
                        };
                        jump_to_line(cx.editor, doc_id, entry.line.saturating_sub(1));
                    },
                )
                .with_preview(|_editor, entry| {
                    entry.existing_path().map(|path| {
                        (
                            path.as_path().into(),
                            Some((entry.line.saturating_sub(1), 0)),
                        )
                    })
                });
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
    Ok(())
}

pub(crate) fn pr_comment(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let body = args.join(" ");
    if !body.trim().is_empty() {
        let target = match comment_target(cx) {
            Ok(target) => target,
            Err(message) => {
                cx.editor.set_error(message);
                return Ok(());
            }
        };
        add_pending_comment(cx.editor, target.anchor, body.trim().to_string());
        return Ok(());
    }

    let target = match comment_target(cx) {
        Ok(target) => target,
        Err(message) => {
            cx.editor.set_error(message);
            return Ok(());
        }
    };

    // `Prompt` is not `Send`, so it must be constructed inside the compositor
    // callback rather than captured from this context.
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_editor: &mut Editor, compositor: &mut crate::compositor::Compositor| {
                let mut prompt = Prompt::new(
                    "comment:".into(),
                    None,
                    ui::completers::none,
                    move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
                        if event != PromptEvent::Validate {
                            return;
                        }
                        let body = input.trim().to_string();
                        if body.is_empty() {
                            cx.editor.set_error("Comment body is empty");
                            return;
                        }
                        add_pending_comment(cx.editor, target.anchor.clone(), body);
                    },
                );
                prompt.recalculate_completion(_editor);
                compositor.push(Box::new(prompt));
            },
        )))
    });
    Ok(())
}

/// Publish the pending comments together with a review verdict
/// (`:review approve|changes|comment [body]`).
pub(crate) fn review(
    cx: &mut compositor::Context,
    args: Args,
    event: PromptEvent,
) -> anyhow::Result<()> {
    if event != PromptEvent::Validate {
        return Ok(());
    }
    let Some(kind) = args.first().map(str::to_ascii_lowercase) else {
        cx.editor
            .set_error("Usage: :review approve|changes|comment [body]");
        return Ok(());
    };
    let event_kind = match kind.as_str() {
        "approve" => "APPROVE",
        "changes" | "request-changes" => "REQUEST_CHANGES",
        "comment" => "COMMENT",
        _ => {
            cx.editor
                .set_error("Unknown review kind; expected approve, changes or comment");
            return Ok(());
        }
    };
    let body: String = args
        .iter()
        .skip(1)
        .map(|arg| arg.as_ref())
        .collect::<Vec<_>>()
        .join(" ");

    let Some((cwd, owner, repo, detail, pending)) = review::with_review_state(|state| {
        (
            state.workspace_root.clone(),
            state.owner.clone(),
            state.repo.clone(),
            state.detail.clone(),
            state.pending.clone(),
        )
    }) else {
        cx.editor
            .set_error("No pull request is loaded; run :pr first");
        return Ok(());
    };
    if pending.is_empty() && body.trim().is_empty() {
        cx.editor.set_error(
            "Nothing to publish: add comments with `:pr-comment` or give a summary body",
        );
        return Ok(());
    }

    cx.editor.set_status(format!("Submitting review ({kind})…"));
    cx.jobs.callback(async move {
        gh::submit_review(
            &cwd,
            &owner,
            &repo,
            &detail,
            event_kind,
            body.trim(),
            &pending,
        )
        .await?;
        // Fetch the submitted comments back so they render as remote ones.
        let comments = gh::fetch_comments(&cwd, &owner, &repo, detail.number).await?;
        Ok(Callback::EditorCompositor(Box::new(move |editor, _| {
            let Some(()) = review::with_review_state(|state| {
                state.comments = comments;
                state.pending.clear();
                state.rendered =
                    render_review(&state.detail, &state.diff, &state.comments, &state.pending);
            }) else {
                return;
            };
            if let Err(error) = open_diff_buffer(editor) {
                editor.set_error(error.to_string());
            }
            editor.set_status("Review submitted");
        })))
    });
    Ok(())
}

#[derive(Clone)]
struct CommentTarget {
    anchor: review::Anchor,
}

struct PrFileEntry {
    display: String,
    path: Option<PathBuf>,
    status: FileStatus,
}

struct CommentEntry {
    /// Author display name (`@login` or `you (pending)`).
    author: String,
    path: PathBuf,
    /// `path:line (side)` display string.
    location: String,
    body: String,
    /// 0-based line number of the anchor in the working-tree file.
    line: usize,
}

impl CommentEntry {
    /// The file path if it exists in the working tree.
    fn existing_path(&self) -> Option<&PathBuf> {
        self.path.exists().then_some(&self.path)
    }
}

/// Collapse a comment body to a single line for the picker column.
fn one_line(body: &str) -> String {
    let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut truncated: String = collapsed.chars().take(80).collect();
    if truncated.chars().count() < collapsed.chars().count() {
        truncated.push('…');
    }
    truncated
}

/// Workspace root of the current document (falls back to the process cwd).
fn workspace_root(cx: &compositor::Context) -> PathBuf {
    doc!(cx.editor).workspace_root().to_path_buf()
}

/// The diff line (of the `:pr-diff` buffer) or the file line (of a reviewed
/// file) under the cursor.
fn comment_target(cx: &compositor::Context) -> Result<CommentTarget, String> {
    let Some(state) = review::with_review_state(|state| {
        (
            state.diff_doc,
            state.workspace_root.clone(),
            state.rendered.line_anchors.clone(),
        )
    }) else {
        return Err("No pull request is loaded; run :pr first".into());
    };
    let (diff_doc, root, line_anchors) = state;

    let (view, doc) = current_ref!(cx.editor);

    // Inside the `:pr-diff` buffer the anchor comes from the rendered lines.
    if Some(view.doc) == diff_doc {
        let line = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        let Some(Some(anchor)) = line_anchors.get(line) else {
            return Err("The cursor is not on a diff line".into());
        };
        return Ok(CommentTarget {
            anchor: anchor.clone(),
        });
    }

    // Inside a real file: the anchor is the cursor line, but only lines that
    // are part of the diff can be commented on.
    let path = doc
        .path()
        .and_then(|path| path.strip_prefix(&root).ok().map(Path::to_string_lossy))
        .ok_or_else(|| "Move the cursor into the PR diff buffer (:pr-diff) first".to_string())?;
    let line = doc
        .selection(view.id)
        .primary()
        .cursor_line(doc.text().slice(..));
    let Some(anchor) = review::with_review_state(|state| {
        state
            .commentable
            .get(path.as_ref())
            .is_some_and(|lines| lines.contains(&((line + 1) as u32)))
            .then(|| review::Anchor {
                path: path.into_owned(),
                side: Side::Right,
                line: (line + 1) as u32,
            })
    })
    .flatten() else {
        return Err(
            "This line is not part of the PR diff; comments must anchor to a changed line".into(),
        );
    };
    Ok(CommentTarget { anchor })
}

/// Add a locally pending comment and refresh the rendered views. Pending
/// comments are only submitted when a review is published with `:review`.
fn add_pending_comment(editor: &mut Editor, anchor: review::Anchor, body: String) {
    let Some(()) = review::with_review_state(|state| {
        state.pending.push(review::PendingComment {
            anchor,
            body: body.clone(),
        });
        state.rendered = render_review(&state.detail, &state.diff, &state.comments, &state.pending);
    }) else {
        editor.set_error("No pull request is loaded; run :pr first");
        return;
    };
    if let Err(error) = open_diff_buffer(editor) {
        editor.set_error(error.to_string());
    }
    editor.set_status(format!(
        "Pending comment added ({}) — publish with `:review approve|changes|comment`",
        review::with_review_state(|state| state.pending.len()).unwrap_or_default()
    ));
}

/// Open the PR list picker and stream results from `gh pr list`.
fn pr_list_picker(cx: &mut compositor::Context) {
    struct PrListData {
        style_dim: helix_view::theme::Style,
    }

    let cwd = workspace_root(cx);
    let style_dim = cx.editor.theme.get("ui.text");

    let columns = [
        PickerColumn::new("pr", |item: &PrListItem, _data: &PrListData| {
            format!("#{}", item.number).into()
        }),
        PickerColumn::new("title", |item: &PrListItem, _data: &PrListData| {
            item.title.clone().into()
        }),
        PickerColumn::new("author", |item: &PrListItem, _data: &PrListData| {
            format!("@{}", item.author.login).into()
        }),
        PickerColumn::new("branch", |item: &PrListItem, data: &PrListData| {
            tui::text::Span::styled(item.head_ref_name.clone(), data.style_dim).into()
        }),
    ];

    // The picker is constructed inside the compositor callback (components
    // are not `Send`); only plain data is captured.
    let picker_cwd = cwd.clone();
    let fetch_cwd = cwd;
    cx.jobs.callback(async move {
        Ok(Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let picker = Picker::new(
                    columns,
                    1,
                    [],
                    PrListData { style_dim },
                    move |cx, item: &PrListItem, _action| {
                        start_review(cx, picker_cwd.clone(), item.number);
                    },
                );
                let injector = picker.injector();
                tokio::spawn(async move {
                    match gh::run_gh(
                        &fetch_cwd,
                        &[
                            "pr",
                            "list",
                            "--json",
                            "number,title,author,headRefName,baseRefName,updatedAt",
                            "--limit",
                            "100",
                        ],
                    )
                    .await
                    {
                        Ok(output) => {
                            match serde_json::from_str::<Vec<PrListItem>>(&output) {
                                Ok(items) => {
                                    for item in items {
                                        if injector.push(item).is_err() {
                                            break; // picker closed
                                        }
                                    }
                                }
                                Err(error) => job::dispatch_blocking(move |editor, _| {
                                    editor.set_error(format!(
                                        "Could not parse `gh pr list` output: {error}"
                                    ));
                                }),
                            }
                        }
                        Err(error) => job::dispatch_blocking(move |editor, _| {
                            editor.set_error(error.to_string());
                        }),
                    }
                });
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
}

/// Check out the PR and load its diff and comments (async), then open the
/// review buffer on the main thread.
fn start_review(cx: &mut compositor::Context, cwd: PathBuf, number: u64) {
    cx.editor
        .set_status(format!("Checking out PR #{number} and loading review…"));
    cx.jobs.callback(checkout_and_review(cwd, number));
}

async fn checkout_and_review(cwd: PathBuf, number: u64) -> anyhow::Result<Callback> {
    let number_string = number.to_string();
    // The checkout goes through git/ssh; an SSH key passphrase is asked via
    // an in-editor prompt (see `review::askpass`).
    #[cfg(unix)]
    review::askpass::run_gh_with_askpass(&cwd, &["pr", "checkout", &number_string]).await?;
    #[cfg(not(unix))]
    gh::run_gh(&cwd, &["pr", "checkout", &number_string]).await?;
    let detail_json = gh::run_gh(
        &cwd,
        &[
            "pr",
            "view",
            &number_string,
            "--json",
            "number,title,author,baseRefName,headRefName,headRefOid,baseRefOid,url",
        ],
    )
    .await?;
    let detail: PrDetail = serde_json::from_str(&detail_json)
        .map_err(|error| anyhow::anyhow!("Could not parse PR info: {error}"))?;
    let (owner, repo) = gh::owner_repo_from_url(&detail.url)
        .ok_or_else(|| anyhow::anyhow!("Could not parse repository from {}", detail.url))?;
    let diff_text = gh::run_gh(&cwd, &["pr", "diff", &number_string]).await?;
    let comments = gh::fetch_comments(&cwd, &owner, &repo, number).await?;

    let diff = parse_unified_diff(&diff_text);
    let rendered = render_review(&detail, &diff, &comments, &[]);
    let file_kinds = file_line_kinds(&diff);
    let commentable = review::commentable_lines(&diff);
    let diff_base_sha = gh::fetch_diff_base_sha(
        &cwd,
        &owner,
        &repo,
        &detail.head_ref_oid,
        &detail.base_ref_oid,
    )
    .await
    .unwrap_or_else(|_| detail.base_ref_oid.clone());

    Ok(Callback::EditorCompositor(Box::new(
        move |editor, compositor| {
            let previous_doc = review::with_review_state(|state| state.diff_doc).flatten();
            review::set_review_state(Some(ReviewState {
                workspace_root: cwd.clone(),
                detail,
                owner,
                repo,
                diff,
                comments,
                rendered,
                diff_doc: previous_doc,
                diff_base_sha,
                file_kinds,
                commentable,
                pending: Vec::new(),
            }));
            // The working tree changed under every open document.
            reload_all_docs(editor);
            // Files that were already open get the PR diff base too; files
            // opened later are handled by the DocumentDidOpen hook.
            prime_diff_bases(editor);
            // The changed-files picker is the review surface; each file opens
            // with the PR diff shown in place. `:pr-diff` still offers the
            // unified overview.
            if let Some((_root, files)) = review_file_entries() {
                push_files_picker(compositor, files);
            }
            let status = review::with_review_state(|state| {
                format!(
                    "PR #{}: {} ({} files) — pick a file to review",
                    state.detail.number,
                    state.detail.title,
                    state.diff.files.len()
                )
            })
            .unwrap_or_default();
            editor.set_status(status);
        },
    )))
}

/// Install the PR diff base into every document that is open in the review
/// workspace and part of the PR.
fn prime_diff_bases(editor: &mut Editor) {
    let targets: Vec<(DocumentId, PathBuf, String, String, String, String)> = editor
        .documents
        .iter()
        .filter_map(|(doc_id, doc)| {
            let path = doc.path()?;
            let (cwd, owner, repo, base_sha, rel) = review::with_review_state(|state| {
                let rel = path
                    .strip_prefix(&state.workspace_root)
                    .ok()?
                    .to_str()?
                    .to_string();
                state.file_kinds.contains_key(&rel).then(|| {
                    (
                        state.workspace_root.clone(),
                        state.owner.clone(),
                        state.repo.clone(),
                        state.diff_base_sha.clone(),
                        rel,
                    )
                })
            })??;
            Some((*doc_id, cwd, owner, repo, base_sha, rel))
        })
        .collect();
    for (doc_id, cwd, owner, repo, base_sha, rel) in targets {
        review::schedule_base_fetch(cwd, owner, repo, base_sha, rel, doc_id);
    }
}

/// Create (or refill) the read-only review buffer with the rendered diff.
fn open_diff_buffer(editor: &mut Editor) -> anyhow::Result<DocumentId> {
    let text = review::with_review_state(|state| state.rendered.text.clone())
        .ok_or_else(|| anyhow::anyhow!("No pull request is loaded; run :pr first"))?;

    // Reuse the existing overview buffer only while it is still open and
    // displayed; otherwise open a fresh one beside the reviewed file so the
    // file (and its LSP-backed view) stays available.
    let doc_id = match review::with_review_state(|state| state.diff_doc) {
        Some(Some(id)) if view_showing(editor, id).is_some() => id,
        _ => {
            // Open beside the reviewed file rather than replacing it, so the
            // file (and its LSP-backed view) stays available while the
            // overview is open.
            let id = editor.new_file(Action::VerticalSplit);
            review::with_review_state(|state| state.diff_doc = Some(id));
            id
        }
    };

    {
        let doc = editor.documents.get_mut(&doc_id).unwrap();
        doc.readonly = true;
        if doc.language_config().is_none() {
            let loader = editor.syn_loader.load();
            let _ = doc.set_language_by_language_id("diff", &loader);
        }
    }

    let view_id = ensure_view_for(editor, doc_id);
    let doc = editor.documents.get_mut(&doc_id).unwrap();
    let new_text = Rope::from(text.as_str());
    let transaction = helix_core::diff::compare_ropes(doc.text(), &new_text);
    doc.apply(&transaction, view_id);
    Ok(doc_id)
}

/// A view currently displaying `doc_id`, if any.
fn view_showing(editor: &Editor, doc_id: DocumentId) -> Option<ViewId> {
    editor
        .tree
        .views()
        .find(|(view, _)| view.doc == doc_id)
        .map(|(view, _)| view.id)
}

/// A view that displays `doc_id`, creating selection state if needed.
fn ensure_view_for(editor: &mut Editor, doc_id: DocumentId) -> ViewId {
    let existing = editor
        .documents
        .get(&doc_id)
        .and_then(|doc| doc.selections().keys().next().copied());
    if let Some(view_id) = existing {
        return view_id;
    }
    let current = view!(editor).id;
    let doc = editor.documents.get_mut(&doc_id).unwrap();
    doc.ensure_view_init(current);
    current
}

/// Reload every document that lives on disk; used after the branch checkout
/// rewrote the working tree. Mirrors `:reload-all` but skips scratch buffers.
fn reload_all_docs(editor: &mut Editor) {
    let scrolloff = editor.config().scrolloff;
    let view_id = view!(editor).id;

    let docs_view_ids: Vec<(DocumentId, Vec<ViewId>)> = editor
        .documents_mut()
        .filter(|doc| doc.path().is_some())
        .map(|doc| {
            let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();
            if view_ids.is_empty() {
                doc.ensure_view_init(view_id);
                view_ids.push(view_id);
            }
            (doc.id(), view_ids)
        })
        .collect();

    for (doc_id, view_ids) in docs_view_ids {
        let doc = doc_mut!(editor, &doc_id);
        let view = view_mut!(editor, view_ids[0]);
        view.sync_changes(doc);

        let trust_full = editor
            .workspace_trust
            .query(
                doc.workspace_root(),
                helix_loader::workspace_trust::TrustQuery::Git,
            )
            .is_trusted();
        if let Err(error) = doc.reload(view, &editor.diff_providers, trust_full) {
            editor.set_error(format!("{error}"));
            continue;
        }

        if let Some(path) = doc.path().map(ToOwned::to_owned) {
            editor
                .language_servers
                .file_event_handler
                .file_changed(path);
        }

        for view_id in view_ids {
            let view = view_mut!(editor, view_id);
            if view.doc.eq(&doc_id) {
                view.sync_changes(doc);
                view.ensure_cursor_in_view(doc, scrolloff);
            }
        }
    }
}

/// Move the primary cursor of a view showing `doc_id` to a 0-based line.
fn jump_to_line(editor: &mut Editor, doc_id: DocumentId, line: usize) {
    let Some(view_id) = editor
        .documents
        .get(&doc_id)
        .and_then(|doc| doc.selections().keys().next().copied())
    else {
        return;
    };
    let doc = editor.documents.get_mut(&doc_id).unwrap();
    let text = doc.text();
    let line = line.min(text.len_lines().saturating_sub(1));
    let pos = text.line_to_char(line);
    doc.set_selection(view_id, helix_core::Selection::single(pos, pos));
    let view = view_mut!(editor, view_id);
    view.ensure_cursor_in_view(doc, 5);
}
