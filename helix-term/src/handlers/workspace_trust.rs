//! Asks for workspace trust the first time a file is opened in a workspace where trust would
//! change something: local config that isn't loaded, or servers that won't start.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use helix_loader::workspace_trust::TrustStatus;
use helix_view::{editor::ConfigEvent, Editor};

use crate::{
    compositor::Compositor,
    runtime::{LayerCommand, UiCommand},
    ui,
};

const LAYER_ID: &str = "workspace-trust";

pub(super) fn attach(editor: &Editor, foreground: crate::runtime::ForegroundEvents) {
    // Workspaces already asked about this session. `deny_once` alone can't stop repeat prompts:
    // a restricted workspace stays restricted whatever is cached.
    let prompted: Arc<Mutex<HashSet<PathBuf>>> = Arc::default();
    editor.lifecycle().on_document_open(move |event| {
        let Some(doc) = event.editor.document(event.doc) else {
            return Ok(());
        };
        let Some(workspace) = doc.workspace_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        let servers_to_load = doc.servers_to_load();
        let trust = &event.editor.workspace_trust;

        // `status`, not `query`: a query reports a stale grant as plain untrusted.
        if trust.status(&workspace) == TrustStatus::Stale {
            event.editor.set_status(
                "Workspace config changed since `:workspace-trust`: it is not loaded until you \
                 run `:workspace-trust` again.",
            );
            return Ok(());
        }
        if !trust.restricted_for_doc(&workspace, servers_to_load) || !trust.prompts_enabled() {
            return Ok(());
        }
        if !prompted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(workspace.clone())
        {
            return Ok(());
        }

        // Dismissing the prompt leaves the workspace untrusted for the session.
        trust.deny_once(&workspace);
        foreground
            .ui(UiCommand::Layer(LayerCommand::WorkspaceTrustPrompt {
                workspace,
            }))
            .map_err(anyhow::Error::from)?;
        Ok(())
    });
}

const TRUST_MESSAGE: &str = "Trust this workspace?

A trusted workspace loads its own config (`.double-helix/`), which can start language servers \
and run commands. Only trust workspaces whose contents you have checked.";

#[derive(Clone, Copy, Debug)]
enum TrustChoice {
    Trust,
    Never,
}

impl ui::menu::Item for TrustChoice {
    type Data = ();

    fn format(&self, _data: &Self::Data) -> ui::menu::Row<'_> {
        match self {
            Self::Trust => "Trust",
            Self::Never => "Never",
        }
        .into()
    }
}

pub(crate) fn push_prompt(compositor: &mut Compositor, workspace: PathBuf) {
    let message = format!("{TRUST_MESSAGE}\n\n{}", workspace.display());
    let select = ui::Select::new(
        message,
        [TrustChoice::Trust, TrustChoice::Never],
        (),
        move |editor, choice, event| {
            if event != ui::PromptEvent::Validate {
                return;
            }
            let saved = match choice {
                TrustChoice::Trust => editor.workspace_trust.trust(&workspace),
                TrustChoice::Never => editor.workspace_trust.exclude(&workspace),
            };
            if let Err(err) = saved {
                editor.set_error(format!(
                    "Saving the workspace trust decision failed (it applies to this session): {err}"
                ));
            }
            // Reload so trusted config and servers apply, or anything loaded before is dropped.
            if let Err(err) = editor.config_events.0.try_send(ConfigEvent::Refresh) {
                log::warn!("workspace trust: couldn't queue a config reload: {err:?}");
            }
        },
    );
    compositor.replace_or_push(LAYER_ID, select);
}
