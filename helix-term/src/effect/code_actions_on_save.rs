//! `code-actions-on-save`: before a save, request each configured code action kind in order,
//! resolve and apply what the server returns, then format and save.
//!
//! Every step sees the document the previous one left (the server has the edits by then), and
//! runs as exit-bound work, so quitting (`:wq`) waits for the whole chain.

use std::collections::VecDeque;

use futures_util::future::BoxFuture;
use helix_core::syntax::config::LanguageServerFeature;
use helix_lsp::{lsp, util::range_to_lsp_range, LanguageServerId, OffsetEncoding};
use helix_view::{DocumentId, Editor};

use crate::runtime::{chain_exit_task, OnSaveFinish, PendingFormatWrite, RuntimeTaskEvent};

type StepFuture = BoxFuture<'static, anyhow::Result<RuntimeTaskEvent>>;

/// The code action kinds `doc_id`'s language wants applied before a save.
pub(crate) fn kinds_for(editor: &Editor, doc_id: DocumentId) -> VecDeque<String> {
    editor
        .document(doc_id)
        .and_then(|doc| doc.language_config())
        .and_then(|config| config.code_actions_on_save.clone())
        .unwrap_or_default()
        .into()
}

/// Request the next kind in `kinds`; once none is left, finish (format, then save).
pub(crate) fn next_step(
    editor: &Editor,
    doc_id: DocumentId,
    mut kinds: VecDeque<String>,
    finish: OnSaveFinish,
) -> StepFuture {
    while let Some(kind) = kinds.pop_front() {
        let Some(doc) = editor.document(doc_id) else {
            break;
        };
        let Some(server) = doc
            .language_servers_with_feature(LanguageServerFeature::CodeAction)
            .next()
        else {
            break;
        };
        let offset_encoding = server.offset_encoding();
        let text = doc.text();
        let range = range_to_lsp_range(
            text,
            helix_core::Range::new(0, text.len_chars()),
            offset_encoding,
        );
        let context = lsp::CodeActionContext {
            diagnostics: Vec::new(),
            only: Some(vec![lsp::CodeActionKind::from(kind.clone())]),
            trigger_kind: Some(lsp::CodeActionTriggerKind::AUTOMATIC),
        };
        let Some(request) = server.code_actions(doc.identifier(), range, context) else {
            continue;
        };
        let (version, server_id) = (doc.version(), server.id());
        return Box::pin(async move {
            let actions = match request.await {
                Ok(actions) => actions.unwrap_or_default(),
                Err(err) => {
                    log::warn!("code-actions-on-save: `{kind}` request failed: {err}");
                    Vec::new()
                }
            };
            Ok(RuntimeTaskEvent::CodeActionsOnSaveResponse {
                doc_id,
                version,
                server_id,
                offset_encoding,
                kind,
                actions,
                remaining: kinds,
                finish,
            })
        });
    }
    Box::pin(async move { Ok(RuntimeTaskEvent::CodeActionsOnSaveDone { doc_id, finish }) })
}

/// Whether `action` is of `requested` kind or a sub-kind of it: servers may ignore `only`,
/// and `source.fixAll` legitimately comes back as `source.fixAll.eslint`.
fn kind_matches(action: &lsp::CodeAction, requested: &str) -> bool {
    action.kind.as_ref().is_some_and(|kind| {
        let kind = kind.as_str();
        kind == requested
            || kind
                .strip_prefix(requested)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

enum ResolveStep {
    Ready(lsp::WorkspaceEdit),
    Resolve(BoxFuture<'static, helix_lsp::Result<lsp::CodeAction>>),
}

/// The server answered for `kind`: resolve the matching actions that left their edit out,
/// keeping server order, then apply them in the next step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_response(
    editor: &mut Editor,
    doc_id: DocumentId,
    version: i32,
    server_id: LanguageServerId,
    offset_encoding: OffsetEncoding,
    kind: String,
    actions: Vec<lsp::CodeActionOrCommand>,
    remaining: VecDeque<String>,
    finish: OnSaveFinish,
) {
    let mut steps = Vec::new();
    let current = editor
        .document(doc_id)
        .is_some_and(|doc| doc.version() == version);
    match (current, editor.language_server_by_id(server_id)) {
        (true, Some(server)) => {
            for action in &actions {
                let lsp::CodeActionOrCommand::CodeAction(action) = action else {
                    continue;
                };
                if action.disabled.is_some() || !kind_matches(action, &kind) {
                    continue;
                }
                if action.edit.is_none() {
                    if let Some(resolve) = server.resolve_code_action(action) {
                        steps.push(ResolveStep::Resolve(Box::pin(resolve)));
                        continue;
                    }
                }
                steps.extend(action.edit.clone().map(ResolveStep::Ready));
            }
        }
        (false, _) => log::debug!("code-actions-on-save: document changed, skipping `{kind}`"),
        (true, None) => {}
    }
    chain_exit_task(async move {
        let mut edits = Vec::new();
        for step in steps {
            match step {
                ResolveStep::Ready(edit) => edits.push(edit),
                ResolveStep::Resolve(resolve) => match resolve.await {
                    Ok(action) => edits.extend(action.edit),
                    Err(err) => log::warn!("code-actions-on-save: resolving failed: {err}"),
                },
            }
        }
        Ok(RuntimeTaskEvent::CodeActionsOnSaveResolved {
            doc_id,
            version,
            offset_encoding,
            edits,
            remaining,
            finish,
        })
    });
}

/// Apply the resolved edits (if the document is still the one they were computed for) and
/// continue with the next kind.
pub(crate) fn on_resolved(
    editor: &mut Editor,
    doc_id: DocumentId,
    version: i32,
    offset_encoding: OffsetEncoding,
    edits: Vec<lsp::WorkspaceEdit>,
    remaining: VecDeque<String>,
    finish: OnSaveFinish,
) {
    if editor
        .document(doc_id)
        .is_some_and(|doc| doc.version() == version)
    {
        for edit in &edits {
            apply_to_document(editor, doc_id, offset_encoding, edit);
        }
    }
    chain_exit_task(next_step(editor, doc_id, remaining, finish));
}

/// Apply the part of `edit` that changes `doc_id`. On-save actions (organize imports,
/// fix-all) edit the file being saved; edits to other files are left out.
fn apply_to_document(
    editor: &mut Editor,
    doc_id: DocumentId,
    offset_encoding: OffsetEncoding,
    edit: &lsp::WorkspaceEdit,
) {
    let Some(doc) = editor.document(doc_id) else {
        return;
    };
    let Some(path) = doc.path().cloned() else {
        return;
    };
    let targets_doc = |uri: &lsp::Url| uri.to_file_path().is_ok_and(|file| file == path);

    let mut text_edits = Vec::new();
    let mut other_files = false;
    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if targets_doc(uri) {
                text_edits.extend(edits.iter().cloned());
            } else {
                other_files = true;
            }
        }
    }
    let document_edits = match &edit.document_changes {
        Some(lsp::DocumentChanges::Edits(edits)) => edits.iter().collect::<Vec<_>>(),
        Some(lsp::DocumentChanges::Operations(operations)) => operations
            .iter()
            .filter_map(|operation| match operation {
                lsp::DocumentChangeOperation::Edit(edit) => Some(edit),
                lsp::DocumentChangeOperation::Op(_) => {
                    other_files = true;
                    None
                }
            })
            .collect(),
        None => Vec::new(),
    };
    for document_edit in document_edits {
        if targets_doc(&document_edit.text_document.uri) {
            text_edits.extend(document_edit.edits.iter().map(|edit| match edit {
                lsp::OneOf::Left(edit) => edit.clone(),
                lsp::OneOf::Right(annotated) => annotated.text_edit.clone(),
            }));
        } else {
            other_files = true;
        }
    }
    if other_files {
        log::warn!("code-actions-on-save: skipped edits to other files or file operations");
    }
    if text_edits.is_empty() {
        return;
    }

    let transaction =
        helix_lsp::util::generate_transaction_from_edits(doc.text(), text_edits, offset_encoding);
    let view_id = editor.get_synced_view_id(doc_id);
    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };
    doc.apply(&transaction, view_id);
    doc.append_changes_to_history(editor.tree.get_mut(view_id));
}

/// All kinds ran: format if asked, then save.
pub(crate) fn on_done(editor: &mut Editor, doc_id: DocumentId, finish: OnSaveFinish) {
    let OnSaveFinish {
        view_id,
        path,
        policy,
        auto_format,
    } = finish;
    let format = auto_format
        .then(|| {
            let doc = editor.document(doc_id)?;
            doc.auto_format(editor)
                .map(|format| (doc.version(), format))
        })
        .flatten();
    match format {
        Some((version, format)) => chain_exit_task(crate::commands::make_format_task_event(
            doc_id,
            version,
            view_id,
            format,
            Some(PendingFormatWrite { path, policy }),
        )),
        None => {
            if let Err(err) = editor.save(doc_id, path, policy) {
                editor.set_error(format!("Error saving: {err}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::kind_matches;
    use helix_lsp::lsp;

    fn action(kind: &str) -> lsp::CodeAction {
        lsp::CodeAction {
            title: String::new(),
            kind: Some(lsp::CodeActionKind::from(kind.to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn kind_matches_the_kind_and_its_sub_kinds_only() {
        assert!(kind_matches(&action("source.fixAll"), "source.fixAll"));
        assert!(kind_matches(
            &action("source.fixAll.eslint"),
            "source.fixAll"
        ));
        assert!(!kind_matches(
            &action("source.fixAllOther"),
            "source.fixAll"
        ));
        assert!(!kind_matches(
            &action("source.organizeImports"),
            "source.fixAll"
        ));
        assert!(!kind_matches(&lsp::CodeAction::default(), "source.fixAll"));
    }
}
