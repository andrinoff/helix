//! Loads the PR diff base into files that are opened while a review is
//! active, so the working tree shows the PR's changes in place (diff gutter,
//! hunk motion, `:reset-diff-change`) and LSP features work on the real
//! files.

use helix_event::register_hook;
use helix_view::events::DocumentDidOpen;
use helix_view::handlers::Handlers;

use crate::review::schedule_base_fetch;
use crate::review::with_review_state;

pub(crate) fn register_hooks(_handlers: &Handlers) {
    let _ = _handlers;
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let doc_id = event.doc;
        let path = doc!(event.editor, &doc_id).path().map(ToOwned::to_owned);
        let Some(path) = path else {
            return Ok(());
        };
        // Only files that are part of the reviewed PR are touched; anything
        // else keeps its normal diff base.
        let Some(Some((cwd, owner, repo, base_sha, rel))) = with_review_state(|state| {
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
        }) else {
            return Ok(());
        };
        schedule_base_fetch(cwd, owner, repo, base_sha, rel, doc_id);
        Ok(())
    });
}
