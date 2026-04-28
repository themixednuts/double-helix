use std::{io::stdin, path::Path};

use anyhow::Error;
use helix_core::Selection;
use helix_lsp::{self};

use crate::{document::DocumentOpenError, Align, Document, DocumentId, View, ViewId};

use super::{Action, CloseError, Editor};

impl Editor {
    pub fn switch(&mut self, id: DocumentId, action: Action) {
        use crate::tree::Layout;

        if !self.documents.contains_key(&id) {
            log::error!("cannot switch to document that does not exist (anymore)");
            return;
        }

        if !matches!(action, Action::Load) {
            self.enter_normal_mode();
        }

        let focus_lost = match action {
            Action::Replace => {
                let (view_id, doc) = focused_ref!(self);
                let remove_empty_scratch = !doc.is_modified()
                    && doc.path().is_none()
                    && !doc.is_persistent_scratch()
                    && id != doc.id
                    && !self
                        .tree
                        .traverse()
                        .any(|(_, view)| view.doc == doc.id && view.id != view_id);

                if doc.path().is_none() || doc.is_persistent_scratch() {
                    log::warn!(
                        "[acp_scratch] switch action={:?} from_doc={:?} to_doc={:?} modified={} persistent={} remove_empty_scratch={} view_id={:?}",
                        action,
                        doc.id,
                        id,
                        doc.is_modified(),
                        doc.is_persistent_scratch(),
                        remove_empty_scratch,
                        view_id
                    );
                }

                let (view_id, doc) = focused!(self);

                let view = self.tree.get_mut(view_id);
                doc.append_changes_to_history(view);

                if remove_empty_scratch {
                    log::warn!(
                        "[acp_scratch] removing empty scratch doc={:?} while switching to {:?}",
                        doc.id,
                        id
                    );
                    let old_id = doc.id;
                    self.documents.remove(&old_id);

                    for (view, _) in self.tree.views_mut() {
                        view.remove_document(&old_id);
                    }
                } else {
                    let view = self.tree.get_mut(view_id);
                    let jump = (view.doc, doc.selection(view_id).clone());
                    view.history.jumps.push(jump);
                    if doc.id != id {
                        view.add_to_history(view.doc);
                        if doc.take_modified_since_accessed()
                            && view.last_modified_docs[0] != Some(view.doc)
                        {
                            view.last_modified_docs = [Some(view.doc), view.last_modified_docs[0]];
                        }
                    }
                }

                self.replace_document_in_view(view_id, id);

                self.dispatch_document_focus_lost(id);
                return;
            }
            Action::Load => {
                let view_id = view!(self).id;
                let doc = doc_mut!(self, &id);
                doc.ensure_view_init(view_id);
                doc.mark_as_focused();
                return;
            }
            Action::HorizontalSplit | Action::VerticalSplit => {
                let focus_lost = self.tree.try_get(self.tree.focus).map(|view| view.doc);
                let view = self
                    .tree
                    .try_get(self.tree.focus)
                    .filter(|view| id == view.doc)
                    .cloned()
                    .unwrap_or_else(|| View::new(id, self.config().gutters.clone()));
                let mut view = view;
                self.bind_view_redraw(&mut view);
                let view_id = self.tree.split(
                    view,
                    match action {
                        Action::HorizontalSplit => Layout::Horizontal,
                        Action::VerticalSplit => Layout::Vertical,
                        _ => unreachable!(),
                    },
                );
                let doc = doc_mut!(self, &id);
                doc.ensure_view_init(view_id);
                doc.mark_as_focused();
                focus_lost
            }
        };

        self._refresh();
        if let Some(focus_lost) = focus_lost {
            self.dispatch_document_focus_lost(focus_lost);
        }
    }

    pub fn new_file_from_document(&mut self, action: Action, doc: Document) -> DocumentId {
        let id = self.new_document(doc);
        self.switch(id, action);
        id
    }

    pub fn open_markdown_scratch(&mut self, action: Action, text: String) -> DocumentId {
        let mut doc = Document::from(
            text.into(),
            None,
            self.config.clone(),
            self.syn_loader.clone(),
        )
        .with_persistent_scratch();
        let _ = doc.set_language_by_language_id("markdown", &self.syn_loader.load());
        self.new_file_from_document(action, doc)
    }

    pub fn switch_document_if_exists(&mut self, id: DocumentId, action: Action) -> bool {
        if self.document(id).is_none() {
            return false;
        }
        self.switch(id, action);
        true
    }

    pub fn new_file(&mut self, action: Action) -> DocumentId {
        self.new_file_from_document(
            action,
            Document::default(self.config.clone(), self.syn_loader.clone()),
        )
    }

    pub fn new_file_welcome(&mut self) -> DocumentId {
        self.new_file_from_document(
            Action::VerticalSplit,
            Document::default(self.config.clone(), self.syn_loader.clone()).with_welcome(),
        )
    }

    pub fn new_file_from_stdin(&mut self, action: Action) -> Result<DocumentId, Error> {
        let (stdin, encoding, has_bom) = crate::document::read_to_string(&mut stdin(), None)?;
        let doc = Document::from(
            helix_core::Rope::default(),
            Some((encoding, has_bom)),
            self.config.clone(),
            self.syn_loader.clone(),
        );
        let doc_id = self.new_file_from_document(action, doc);
        let doc = doc_mut!(self, &doc_id);
        let view = view_mut!(self);
        doc.ensure_view_init(view.id);
        let transaction =
            helix_core::Transaction::insert(doc.text(), doc.selection(view.id), stdin.into())
                .with_selection(Selection::point(0));
        doc.apply(&transaction, view.id);
        doc.append_changes_to_history(view);
        Ok(doc_id)
    }

    pub fn document_id_by_path(&self, path: &Path) -> Option<DocumentId> {
        self.document_by_path(path).map(|doc| doc.id)
    }

    pub fn open(&mut self, path: &Path, action: Action) -> Result<DocumentId, DocumentOpenError> {
        let path = helix_stdx::path::canonicalize(path);
        let id = self.document_id_by_path(&path);

        let id = if let Some(id) = id {
            id
        } else {
            let mut doc = Document::open(
                &path,
                None,
                true,
                self.config.clone(),
                self.syn_loader.clone(),
            )?;

            let diagnostics =
                Editor::doc_diagnostics(&self.language_servers, &self.diagnostics, &doc);
            doc.replace_diagnostics(diagnostics, &[], None);

            let diff_base = self.diff_providers.get_diff_base(&path);
            let version_control_head = self.diff_providers.get_current_head_name(&path);

            let id = self.new_document(doc);
            let doc = self
                .documents
                .get_mut(&id)
                .expect("newly inserted document must exist");
            if let Some(diff_base) = diff_base {
                doc.set_diff_base(diff_base);
            }
            doc.set_version_control_head(version_control_head);

            self.launch_language_servers(id);

            self.dispatch_document_open(id, &path);

            id
        };

        self.switch(id, action);

        Ok(id)
    }

    pub fn open_uri_path(
        &mut self,
        path: &Path,
        action: Action,
    ) -> Result<DocumentId, DocumentOpenError> {
        self.open(path, action)
    }

    pub fn show_document(
        &mut self,
        request: super::ShowDocumentRequest,
    ) -> Result<(), DocumentOpenError> {
        let super::ShowDocumentRequest {
            path,
            action,
            selection,
            offset_encoding,
        } = request;

        let doc_id = self.open_uri_path(&path, action)?;
        let Some(range) = selection else {
            return Ok(());
        };

        let doc = doc_mut!(self, &doc_id);
        if let Some(new_range) =
            helix_lsp::util::lsp_range_to_range(doc.text(), range, offset_encoding)
        {
            let view = view_mut!(self);
            doc.set_selection(view.id, Selection::single(new_range.head, new_range.anchor));
            if action.align_view(view, doc.id()) {
                crate::align_view(doc, view, Align::Center);
            }
        } else {
            log::warn!("lsp position out of bounds - {:?}", range);
        }

        Ok(())
    }

    pub fn close(&mut self, id: ViewId) {
        for doc in self.documents_mut() {
            doc.remove_view(id);
        }
        self.tree.remove(id);
        self._refresh();
    }

    pub fn close_document(
        &mut self,
        doc_id: DocumentId,
        policy: super::ClosePolicy,
    ) -> Result<(), CloseError> {
        let doc = match self.documents.get(&doc_id) {
            Some(doc) => doc,
            None => return Err(CloseError::DoesNotExist),
        };
        if !policy.should_discard_modified() && doc.is_modified() {
            return Err(CloseError::BufferModified(doc.display_name().into_owned()));
        }

        self.saves.remove(&doc_id);

        enum CloseAction {
            Close(ViewId),
            ReplaceDoc(ViewId, DocumentId),
        }

        let actions: Vec<CloseAction> = self
            .tree
            .views_mut()
            .filter_map(|(view, _focus)| {
                view.remove_document(&doc_id);

                if view.doc == doc_id {
                    if let Some(prev_doc) = view.docs_access_history.pop() {
                        Some(CloseAction::ReplaceDoc(view.id, prev_doc))
                    } else {
                        Some(CloseAction::Close(view.id))
                    }
                } else {
                    None
                }
            })
            .collect();

        for action in actions {
            match action {
                CloseAction::Close(view_id) => self.close(view_id),
                CloseAction::ReplaceDoc(view_id, doc_id) => {
                    self.replace_document_in_view(view_id, doc_id);
                }
            }
        }

        let doc = self.documents.remove(&doc_id).unwrap();

        if self.tree.views().next().is_none() {
            let doc_id = self
                .documents
                .iter()
                .map(|(&doc_id, _)| doc_id)
                .next()
                .unwrap_or_else(|| {
                    self.new_document(Document::default(
                        self.config.clone(),
                        self.syn_loader.clone(),
                    ))
                });
            let mut view = View::new(doc_id, self.config().gutters.clone());
            self.bind_view_redraw(&mut view);
            let view_id = self.tree.insert(view);
            let _ = self.track_tree_surface(view_id);
            let doc = doc_mut!(self, &doc_id);
            doc.ensure_view_init(view_id);
            doc.mark_as_focused();
        }

        self._refresh();

        self.dispatch_document_close(doc);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command, sync::Arc};

    use arc_swap::ArcSwap;
    use helix_core::syntax;

    use super::*;
    use crate::{editor::Config, graphics::Rect, handlers::Handlers, theme, View};

    fn exec_git(args: &[&str], repo: &Path) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env("GIT_TERMINAL_PROMPT", "false")
            .env("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_NAME", "author")
            .env("GIT_COMMITTER_DATE", "2000-01-02 00:00:00 +0000")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_COMMITTER_NAME", "committer")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
            .env("GIT_CONFIG_VALUE_0", "false")
            .env("GIT_CONFIG_KEY_1", "init.defaultBranch")
            .env("GIT_CONFIG_VALUE_1", "main")
            .output()
            .unwrap_or_else(|_| panic!("`git {args:?}` failed"));

        if !output.status.success() {
            println!("{}", String::from_utf8_lossy(&output.stdout));
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            panic!("`git {args:?}` failed");
        }
    }

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let handlers = Handlers::dummy();
        let mut editor = Editor::new(
            Rect::new(0, 0, 120, 40),
            theme_loader,
            syn_loader,
            config,
            runtime,
            handlers,
        );
        let doc_id = editor.new_document(Document::default(
            editor.config.clone(),
            editor.syn_loader.clone(),
        ));
        let mut view = View::new(doc_id, editor.config().gutters.clone());
        editor.bind_view_redraw(&mut view);
        let view_id = editor.tree.insert(view);
        let _ = editor.track_tree_surface(view_id);
        let doc = crate::doc_mut!(editor, &doc_id);
        doc.ensure_view_init(view_id);
        doc.mark_as_focused();
        editor
    }

    #[test]
    fn open_git_document_binds_redraw_before_vcs_diff() {
        let tokio = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = tokio.enter();
        let runtime = helix_runtime::Runtime::new(tokio.handle().clone());
        let repo = tempfile::tempdir().expect("create temp git repo");
        exec_git(&["init"], repo.path());

        let file = repo.path().join("tracked.txt");
        std::fs::write(&file, "base\n").expect("write base file");
        exec_git(&["add", "tracked.txt"], repo.path());
        exec_git(&["commit", "-m", "initial"], repo.path());
        std::fs::write(&file, "changed\n").expect("write changed file");

        let mut editor = test_editor(runtime);
        let doc_id = editor.open(&file, Action::Replace).expect("open file");
        let doc = editor.document(doc_id).expect("document");

        assert!(doc.diff_handle().is_some());
    }
}
