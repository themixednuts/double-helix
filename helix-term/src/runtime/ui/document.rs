use std::path::PathBuf;

use helix_core::Syntax;
use helix_view::{DocumentId, Editor};

use super::command::DocumentCommand;

fn apply_syntax(
    editor: &mut Editor,
    document: DocumentId,
    path: PathBuf,
    version: i32,
    syntax: Syntax,
) {
    let requested_path = helix_stdx::path::canonicalize(&path);
    let applied = {
        let Some(doc) = editor.document_mut(document) else {
            log::info!(
                "[document_syntax] apply_skip reason=missing_document doc={:?} path={}",
                document,
                requested_path.display()
            );
            return;
        };
        let current_path = doc.path().map(helix_stdx::path::canonicalize);
        if current_path.as_deref() != Some(requested_path.as_path()) {
            log::info!(
                "[document_syntax] apply_skip reason=path_mismatch doc={:?} requested={} current={}",
                document,
                requested_path.display(),
                current_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| String::from("<scratch>")),
            );
            return;
        }
        if doc.version() != version {
            log::info!(
                "[document_syntax] apply_skip reason=version_mismatch doc={:?} path={} requested_version={} current_version={}",
                document,
                requested_path.display(),
                version,
                doc.version(),
            );
            return;
        }
        if doc.has_syntax() {
            log::info!(
                "[document_syntax] apply_skip reason=already_has_syntax doc={:?} path={}",
                document,
                requested_path.display()
            );
            return;
        }

        doc.set_syntax(Some(syntax));
        true
    };

    if applied {
        editor.mark_redraw_pending();
        editor.request_redraw();
        log::info!(
            "[document_syntax] apply_done doc={:?} path={} version={}",
            document,
            requested_path.display(),
            version,
        );
    }
}

pub(crate) fn apply_document_command(editor: &mut Editor, cmd: DocumentCommand) {
    match cmd {
        DocumentCommand::ApplySyntax {
            document,
            path,
            version,
            syntax,
        } => apply_syntax(editor, document, path, version, syntax),
    }
}
