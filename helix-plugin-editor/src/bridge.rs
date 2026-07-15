//! Concrete capability trait implementations that bridge the contract to
//! `helix-view::Editor`.
//!
//! [`EditorQueryBridge`] implements [`PluginQueryHost`] (read-only access).
//! [`EditorMutationBridge`] implements [`PluginMutationHost`] (mutations).
//!
//! UI, panel, command, and event hosts are frontend-dependent and will be
//! implemented in `helix-term` (or another frontend crate) in a later phase.

use helix_core::Tendril;
use helix_view::Editor;

use crate::adapt;
use helix_plugin_api::errors::{ContractError, ContractResult};
use helix_plugin_api::handles::{DocumentHandle, FloatHandle, PluginId, ThreadHandle, ViewHandle};
use helix_plugin_api::host::{
    PluginAssistantMutationHost, PluginAssistantQueryHost, PluginFacadeMutationHost,
    PluginFacadeQueryHost, PluginFloatHost, PluginMutationHost, PluginQueryHost, PluginSplitHost,
    PluginTabHost, PluginWorkspaceQueryHost,
};
use helix_plugin_api::metadata::ApiMetadata;
use helix_plugin_api::requests::*;
use helix_plugin_api::snapshots::*;

// ---------------------------------------------------------------------------
// Query bridge
// ---------------------------------------------------------------------------

/// Read-only bridge from the plugin contract to `&Editor`.
pub struct EditorQueryBridge<'a> {
    pub editor: &'a Editor,
}

impl<'a> EditorQueryBridge<'a> {
    pub fn new(editor: &'a Editor) -> Self {
        Self { editor }
    }

    /// Resolve a document handle and return a reference to the document.
    fn doc(&self, handle: DocumentHandle) -> ContractResult<&helix_view::Document> {
        let id = adapt::resolve_document(self.editor, handle)?;
        self.editor
            .documents
            .get(&id)
            .ok_or_else(|| ContractError::stale_handle(handle.to_string()))
    }

    /// Get the focused view ID (may not exist in edge cases).
    fn focused_view_id(&self) -> Option<helix_view::ViewId> {
        let id = self.editor.tree.focus;
        self.editor.tree.try_get(id).map(|_| id)
    }
}

impl EditorQueryBridge<'_> {
    /// Snapshot the full split tree topology (read-only).
    pub fn split_tree(&self) -> SplitTreeSnapshot {
        adapt::split_tree_snapshot(self.editor)
    }

    /// List tabs in a view's tab group (read-only).
    pub fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot> {
        list_tabs_impl(self.editor, view)
    }
}

impl PluginQueryHost for EditorQueryBridge<'_> {
    fn api_metadata(&self) -> ApiMetadata {
        ApiMetadata::default()
    }

    fn focused_document(&self) -> Option<DocumentHandle> {
        self.focused_view_id()
            .and_then(|vid| self.editor.tree.try_get(vid))
            .map(|view| adapt::document_handle(view.doc))
    }

    fn focused_view(&self) -> Option<ViewHandle> {
        self.focused_view_id().map(adapt::view_handle)
    }

    fn list_documents(&self) -> Vec<DocumentHandle> {
        self.editor
            .documents
            .keys()
            .map(|id| adapt::document_handle(*id))
            .collect()
    }

    fn list_views(&self) -> Vec<ViewHandle> {
        self.editor
            .tree
            .views()
            .map(|(view, _)| adapt::view_handle(view.id))
            .collect()
    }

    fn language_servers(&self) -> ContractResult<Vec<LanguageServerSnapshot>> {
        Ok(self
            .editor
            .language_server_client_names()
            .zip(self.editor.language_server_client_ids())
            .map(|(name, id)| LanguageServerSnapshot {
                id,
                name: name.to_owned(),
            })
            .collect())
    }

    fn document_snapshot(&self, handle: DocumentHandle) -> ContractResult<DocumentSnapshot> {
        let doc_id = adapt::resolve_document(self.editor, handle)?;
        let doc = self
            .editor
            .documents
            .get(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(handle.to_string()))?;
        let view_id = visible_view_for_document(self.editor, doc_id)
            .or_else(|| self.focused_view_id())
            .unwrap_or(self.editor.tree.focus);
        Ok(adapt::document_snapshot(doc, view_id, self.editor.mode))
    }

    fn view_snapshot(&self, handle: ViewHandle) -> ContractResult<ViewSnapshot> {
        let view_id = adapt::resolve_view(self.editor, handle)?;
        let view = self.editor.tree.get(view_id);
        let doc = self
            .editor
            .documents
            .get(&view.doc)
            .ok_or_else(|| ContractError::stale_handle(format!("document for {handle}")))?;
        Ok(adapt::view_snapshot(view, doc))
    }

    fn workspace_snapshot(&self) -> WorkspaceSnapshot {
        adapt::workspace_snapshot(self.editor)
    }

    fn theme_snapshot(&self) -> ThemeSnapshot {
        adapt::theme_snapshot(self.editor)
    }

    fn diagnostics(&self, handle: DocumentHandle) -> ContractResult<DiagnosticSnapshot> {
        let doc = self.doc(handle)?;
        Ok(adapt::diagnostic_snapshot(doc))
    }

    fn document_text(&self, handle: DocumentHandle) -> ContractResult<String> {
        let doc = self.doc(handle)?;
        Ok(doc.text().to_string())
    }

    fn document_line(&self, handle: DocumentHandle, line: usize) -> ContractResult<String> {
        let doc = self.doc(handle)?;
        let text = doc.text();
        if line >= text.len_lines() {
            return Err(ContractError::invalid_request(format!(
                "line {line} out of range (document has {} lines)",
                text.len_lines()
            )));
        }
        Ok(text.line(line).to_string())
    }
}

impl PluginFacadeQueryHost for EditorQueryBridge<'_> {
    fn split_tree(&self) -> SplitTreeSnapshot {
        EditorQueryBridge::split_tree(self)
    }

    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot> {
        EditorQueryBridge::list_tabs(self, view)
    }

    fn editor_config(&self) -> ContractResult<EditorConfigSnapshot> {
        let config = self.editor.config();
        Ok(EditorConfigSnapshot {
            scrolloff: config.scrolloff,
            mouse: config.mouse,
            cursorline: config.cursorline,
            cursorcolumn: config.cursorcolumn,
            auto_format: config.auto_format,
            auto_completion: config.auto_completion,
            auto_info: config.auto_info,
            line_number: match config.line_number {
                helix_view::editor::LineNumber::Absolute => LineNumberMode::Absolute,
                helix_view::editor::LineNumber::Relative => LineNumberMode::Relative,
            },
        })
    }

    fn terminal_size(&self) -> ContractResult<TerminalSizeSnapshot> {
        let area = self.editor.tree.area();
        Ok(TerminalSizeSnapshot {
            width: area.width,
            height: area.height,
        })
    }

    fn read_register(&self, name: char) -> ContractResult<Vec<String>> {
        Ok(self
            .editor
            .read_register(name)
            .map(|values| values.map(|value| value.into_owned()).collect())
            .unwrap_or_default())
    }
}

impl PluginFacadeMutationHost for EditorMutationBridge<'_> {
    fn write_register(&mut self, name: char, values: Vec<String>) -> ContractResult<()> {
        self.editor
            .write_register(name, values)
            .map_err(|error| ContractError::internal(error.to_string()))
    }

    fn request_redraw(&mut self) {
        self.editor.request_redraw();
    }
}

// ---------------------------------------------------------------------------
// Mutation bridge
// ---------------------------------------------------------------------------

/// Mutable bridge from the plugin contract to `&mut Editor`.
pub struct EditorMutationBridge<'a> {
    pub editor: &'a mut Editor,
}

impl<'a> EditorMutationBridge<'a> {
    pub fn new(editor: &'a mut Editor) -> Self {
        Self { editor }
    }
}

impl PluginMutationHost for EditorMutationBridge<'_> {
    fn apply_edit(&mut self, req: ApplyEditRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;

        // Sort edits in reverse order so earlier edits don't shift later positions.
        let mut edits = req.edits;
        edits.sort_by(|a, b| {
            b.start
                .line
                .cmp(&a.start.line)
                .then(b.start.column.cmp(&a.start.column))
        });

        let transaction = {
            let doc = self
                .editor
                .documents
                .get(&doc_id)
                .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;

            let text = doc.text();

            // Convert Position edits to char-index Changes.
            let changes: Vec<(usize, usize, Option<Tendril>)> = {
                let mut result = Vec::with_capacity(edits.len());
                for edit in &edits {
                    let from = position_to_char(text, &edit.start)?;
                    let to = position_to_char(text, &edit.end)?;
                    if from > to {
                        return Err(ContractError::invalid_request(
                            "edit start is after edit end",
                        ));
                    }
                    let tendril = if edit.new_text.is_empty() {
                        None
                    } else {
                        Some(Tendril::from(edit.new_text.as_str()))
                    };
                    result.push((from, to, tendril));
                }
                // Transaction::change expects changes in forward order.
                result.reverse();
                result
            };

            helix_core::Transaction::change(text, changes.into_iter())
        };

        let view_id = resolve_operation_view_for_document(self.editor, doc_id)?;

        let doc = self
            .editor
            .documents
            .get_mut(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;
        let success = doc.apply(&transaction, view_id);
        if success {
            Ok(())
        } else {
            Err(ContractError::InternalError {
                message: "transaction apply failed".into(),
            })
        }
    }

    fn set_selection(&mut self, req: SetSelectionRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        let view_id = match req.view {
            Some(vh) => {
                let view_id = adapt::resolve_view(self.editor, vh)?;
                if self.editor.tree.get(view_id).doc != doc_id {
                    return Err(ContractError::invalid_request(format!(
                        "{vh} does not show {}",
                        req.document
                    )));
                }
                view_id
            }
            None => resolve_operation_view_for_document(self.editor, doc_id)?,
        };

        if req.selections.is_empty() {
            return Err(ContractError::invalid_request(
                "selections must not be empty",
            ));
        }

        let doc = self
            .editor
            .documents
            .get(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;
        let text = doc.text();

        let ranges: Vec<helix_core::selection::Range> = req
            .selections
            .iter()
            .map(|sel| {
                let anchor = position_to_char(text, &sel.anchor)?;
                let head = position_to_char(text, &sel.head)?;
                Ok(helix_core::selection::Range::new(anchor, head))
            })
            .collect::<ContractResult<Vec<_>>>()?;

        let selection = helix_core::Selection::new(ranges.into(), 0);

        let doc = self
            .editor
            .documents
            .get_mut(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;
        doc.set_selection(view_id, selection);
        Ok(())
    }

    fn save_document(&mut self, req: SaveDocumentRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        let doc = self
            .editor
            .documents
            .get(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;

        if !req.force && !doc.is_modified() {
            return Ok(());
        }

        let policy = if req.force {
            helix_view::editor::SavePolicy::Overwrite
        } else {
            helix_view::editor::SavePolicy::Safe
        };

        // Use the editor-level save which handles async work spawning and LSP notifications.
        self.editor
            .save::<std::path::PathBuf>(doc_id, None, policy)
            .map_err(|e| ContractError::InternalError {
                message: e.to_string(),
            })?;
        Ok(())
    }

    fn focus_view(&mut self, req: FocusViewRequest) -> ContractResult<()> {
        let view_id = adapt::resolve_view(self.editor, req.view)?;
        self.editor.tree.focus = view_id;
        Ok(())
    }

    fn set_annotations(&mut self, req: SetAnnotationsRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        let doc = self
            .editor
            .documents
            .get(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;

        // Convert contract annotations to helix_view::document::PluginAnnotation,
        // resolving line:col positions against the document's current text.
        let text = doc.text();
        let converted: Vec<helix_view::document::PluginAnnotation> = req
            .annotations
            .into_iter()
            .map(|annot| helix_view::document::PluginAnnotation {
                char_idx: adapt::position_to_char(text, annot.position),
                text: annot.text,
                style: None,
                fg: annot.style.fg.map(adapt::color_to_hex),
                bg: annot.style.bg.map(adapt::color_to_hex),
                offset: annot.offset,
                is_line: annot.is_line,
                virt_line_idx: annot.virtual_line,
                dropped_text: annot.dropped_text,
            })
            .collect();

        // Collect target view IDs (all views showing this document) before
        // borrowing the editor mutably to set annotations.
        let view_ids: Vec<helix_view::ViewId> = self
            .editor
            .tree
            .views()
            .filter_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
            .collect();

        if view_ids.is_empty() {
            // Nothing to do — document not currently displayed. This is not
            // an error: the plugin may re-call once the document is shown.
            return Ok(());
        }

        // Scope annotations by plugin identity so different plugins' overlays
        // coexist without stomping each other.
        let plugin_id = req.plugin.raw().to_string();
        let doc = self
            .editor
            .documents
            .get_mut(&doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;

        // Replace the scope in the first view; clone the Vec for each
        // additional view (if any) to avoid unnecessary work in the common
        // single-view case.
        let mut views = view_ids.into_iter();
        let Some(first) = views.next() else {
            return Ok(());
        };
        for view_id in views {
            doc.set_plugin_annotations(view_id, plugin_id.clone(), converted.clone());
        }
        doc.set_plugin_annotations(first, plugin_id, converted);
        Ok(())
    }

    fn set_status(&mut self, req: SetStatusRequest) -> ContractResult<()> {
        self.editor.set_status(req.message);
        Ok(())
    }

    fn undo(&mut self, req: UndoRequest) -> ContractResult<bool> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        if !self.editor.documents.contains_key(&doc_id) {
            return Err(ContractError::stale_handle(req.document.to_string()));
        }
        let view_id = resolve_operation_view_for_document(self.editor, doc_id)?;
        Ok(self
            .editor
            .with_view_doc_mut(view_id, doc_id, |view, doc| doc.undo(view)))
    }

    fn redo(&mut self, req: RedoRequest) -> ContractResult<bool> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        if !self.editor.documents.contains_key(&doc_id) {
            return Err(ContractError::stale_handle(req.document.to_string()));
        }
        let view_id = resolve_operation_view_for_document(self.editor, doc_id)?;
        Ok(self
            .editor
            .with_view_doc_mut(view_id, doc_id, |view, doc| doc.redo(view)))
    }

    fn select_all(&mut self, req: SelectAllRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        let view_id = self
            .editor
            .tree
            .views()
            .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
            .ok_or_else(|| ContractError::not_found("view displaying document"))?;
        let len = self
            .editor
            .document(doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?
            .text()
            .len_chars();
        let document = self
            .editor
            .document_mut(doc_id)
            .ok_or_else(|| ContractError::stale_handle(req.document.to_string()))?;
        document.ensure_view_init(view_id);
        document.set_selection(view_id, helix_core::Selection::single(0, len));
        Ok(())
    }

    fn set_mode(&mut self, req: SetModeRequest) -> ContractResult<()> {
        use helix_plugin_api::snapshots::EditMode;
        match req.mode {
            EditMode::Normal => self.editor.enter_normal_mode(),
            EditMode::Insert => self.editor.enter_insert_mode(),
            EditMode::Select => self.editor.enter_select_mode(),
        }
        Ok(())
    }

    fn close_view(&mut self, req: CloseViewRequest) -> ContractResult<()> {
        let view_id = adapt::resolve_view(self.editor, req.view)?;
        self.editor.close(view_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Split bridge
// ---------------------------------------------------------------------------

impl PluginSplitHost for EditorMutationBridge<'_> {
    fn split_view(&mut self, req: SplitViewRequest) -> ContractResult<ViewHandle> {
        use helix_view::editor::Action;

        let action = match req.direction {
            SplitDirection::Right | SplitDirection::Left => Action::VerticalSplit,
            SplitDirection::Down | SplitDirection::Up => Action::HorizontalSplit,
        };
        let view_id = resolve_view_or_focused(self.editor, req.view)?;
        let preserve_focus = req.view.is_some() && view_id != self.editor.tree.focus;
        let doc_id = match req.document {
            Some(doc_handle) => adapt::resolve_document(self.editor, doc_handle)?,
            None => self.editor.tree.get(view_id).doc,
        };

        let new_view_id = with_view_scope(self.editor, view_id, preserve_focus, |editor| {
            editor.switch(doc_id, action);
            Ok(editor.tree.focus)
        })?;
        Ok(adapt::view_handle(new_view_id))
    }

    fn focus_direction(
        &mut self,
        req: FocusDirectionRequest,
    ) -> ContractResult<Option<ViewHandle>> {
        let direction = contract_to_tree_direction(req.direction);
        self.editor.focus_direction(direction);
        let view_id = self.editor.tree.focus;
        Ok(Some(adapt::view_handle(view_id)))
    }

    fn swap_split(&mut self, req: SwapSplitRequest) -> ContractResult<()> {
        let direction = contract_to_tree_direction(req.direction);
        self.editor.swap_split_in_direction(direction);
        Ok(())
    }

    fn resize_split(&mut self, req: ResizeSplitRequest) -> ContractResult<()> {
        use helix_view::tree::{Dimension, Resize};

        let count = match req.amount {
            ResizeAmount::Grow(n) | ResizeAmount::Shrink(n) => n,
        };
        let view_id = resolve_view_or_focused(self.editor, req.view)?;
        let preserve_focus = req.view.is_some() && view_id != self.editor.tree.focus;

        with_view_scope(self.editor, view_id, preserve_focus, |editor| {
            for _ in 0..count {
                let resize = match req.amount {
                    ResizeAmount::Grow(_) => Resize::Grow,
                    ResizeAmount::Shrink(_) => Resize::Shrink,
                };
                let dim = match req.dimension {
                    ResizeDimension::Width => Dimension::Width,
                    ResizeDimension::Height => Dimension::Height,
                };
                editor.resize_buffer(resize, dim);
            }
            Ok(())
        })
    }

    fn transpose(&mut self, req: TransposeSplitRequest) -> ContractResult<()> {
        // `transpose_view` operates on the currently focused view. If the
        // caller requested a different view, temporarily shift focus there
        // and restore afterwards so the operation is scoped as requested.
        if let Some(view_handle) = req.view {
            let target = adapt::resolve_view(self.editor, view_handle)?;
            let prev = self.editor.tree.focus;
            if target != prev {
                self.editor.tree.focus = target;
                self.editor.transpose_view();
                // Restore focus if the previous view still exists.
                if self.editor.tree.contains(prev) {
                    self.editor.tree.focus = prev;
                }
            } else {
                self.editor.transpose_view();
            }
        } else {
            self.editor.transpose_view();
        }
        Ok(())
    }

    fn split_tree(&self) -> SplitTreeSnapshot {
        adapt::split_tree_snapshot(self.editor)
    }
}

// ---------------------------------------------------------------------------
// Tab bridge (per-view document switching as tab groups)
// ---------------------------------------------------------------------------

impl PluginTabHost for EditorMutationBridge<'_> {
    fn open_tab(&mut self, req: OpenTabRequest) -> ContractResult<()> {
        let doc_id = adapt::resolve_document(self.editor, req.document)?;
        let view_id = resolve_view_or_focused(self.editor, req.view)?;

        if req.focus {
            return with_view_scope(self.editor, view_id, false, |editor| {
                editor.switch(doc_id, helix_view::editor::Action::Replace);
                Ok(())
            });
        }

        let view = self.editor.tree.get_mut(view_id);
        if view.doc != doc_id {
            view.add_to_history(doc_id);
        }
        Ok(())
    }

    fn close_tab(&mut self, req: CloseTabRequest) -> ContractResult<()> {
        let view_id = resolve_view_or_focused(self.editor, req.view)?;
        // `None` closes the active tab (index 0).
        let doc_id = tab_doc_at_index(self.editor, view_id, req.index.unwrap_or(0))?;
        self.editor
            .close_document(doc_id, helix_view::editor::ClosePolicy::ProtectModified)
            .map_err(|e| {
                use helix_view::editor::CloseError;
                match e {
                    CloseError::DoesNotExist => ContractError::not_found("document"),
                    CloseError::BufferModified(name) => {
                        ContractError::invalid_request(format!("buffer modified: {name}"))
                    }
                    CloseError::SaveError(err) => ContractError::internal(err.to_string()),
                }
            })
    }

    fn focus_tab(&mut self, req: FocusTabRequest) -> ContractResult<()> {
        // Index 0 is the already-active tab — no work needed.
        if req.index == 0 && req.view.is_none() {
            return Ok(());
        }
        let view_id = resolve_view_or_focused(self.editor, req.view)?;
        let doc_id = tab_doc_at_index(self.editor, view_id, req.index)?;

        // Switch focus to the target view if it isn't already focused, then
        // replace its document.
        if self.editor.tree.focus != view_id {
            self.editor.tree.focus = view_id;
        }
        self.editor
            .switch(doc_id, helix_view::editor::Action::Replace);
        Ok(())
    }

    fn cycle_tab(&mut self, req: CycleTabRequest) -> ContractResult<()> {
        let view_id = resolve_view_or_focused(self.editor, req.view)?;
        let preserve_focus = req.view.is_some() && view_id != self.editor.tree.focus;
        let doc_id = {
            let view = self.editor.tree.get(view_id);
            match req.direction {
                TabCycleDirection::Next => view.docs_access_history.last().copied(),
                TabCycleDirection::Previous => {
                    let len = view.docs_access_history.len();
                    (len >= 2).then_some(view.docs_access_history[len - 2])
                }
            }
        };

        let Some(doc_id) = doc_id else {
            return Ok(());
        };

        with_view_scope(self.editor, view_id, preserve_focus, |editor| {
            editor.switch(doc_id, helix_view::editor::Action::Replace);
            Ok(())
        })
    }

    fn list_tabs(&self, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot> {
        list_tabs_impl(self.editor, view)
    }
}

// ---------------------------------------------------------------------------
// Float bridge
// ---------------------------------------------------------------------------

impl PluginFloatHost for EditorMutationBridge<'_> {
    fn create_float(
        &mut self,
        plugin: PluginId,
        req: CreateFloatRequest,
    ) -> ContractResult<FloatHandle> {
        let placement = contract_to_model_placement(self.editor, &req.placement)?;
        let plugin_owner = plugin_owner_key(plugin);
        let content = contract_to_model_content(self.editor, req.content)?;

        let float_id = self.editor.model.create_float(
            content,
            placement,
            req.title,
            req.dismissible,
            req.focus,
        );
        if let Some(entry) = self.editor.model.float_mut(float_id) {
            entry.owner = Some(plugin_owner);
        }

        Ok(adapt::float_handle(float_id))
    }

    fn update_float(&mut self, plugin: PluginId, req: UpdateFloatRequest) -> ContractResult<()> {
        let placement = req
            .placement
            .as_ref()
            .map(|placement| contract_to_model_placement(self.editor, placement))
            .transpose()?;
        let float_id = adapt::resolve_float(&self.editor.model, req.float)?;
        let plugin_owner = plugin_owner_key(plugin);
        let owner = self
            .editor
            .model
            .float(float_id)
            .ok_or_else(|| ContractError::stale_handle(req.float.to_string()))?
            .owner
            .as_deref();
        if owner != Some(plugin_owner.as_str()) {
            return Err(ContractError::permission_denied(format!(
                "plugin {plugin} does not own {}",
                req.float
            )));
        }
        let content = req
            .content
            .map(|content| contract_to_model_content(self.editor, content))
            .transpose()?;
        let entry = self
            .editor
            .model
            .float_mut(float_id)
            .ok_or_else(|| ContractError::stale_handle(req.float.to_string()))?;

        if let Some(title) = req.title {
            entry.title = title;
        }
        if let Some(placement) = placement {
            entry.placement = placement;
        }
        if let Some(content) = content {
            entry.content = content;
        }
        Ok(())
    }

    fn close_float(&mut self, plugin: PluginId, req: CloseFloatRequest) -> ContractResult<()> {
        let float_id = adapt::resolve_float(&self.editor.model, req.float)?;
        let plugin_owner = plugin_owner_key(plugin);
        let entry = self
            .editor
            .model
            .float(float_id)
            .ok_or_else(|| ContractError::stale_handle(req.float.to_string()))?;
        if entry.owner.as_deref() != Some(plugin_owner.as_str()) {
            return Err(ContractError::permission_denied(format!(
                "plugin {plugin} does not own {}",
                req.float
            )));
        }
        self.editor.model.close_float(float_id);
        Ok(())
    }

    fn list_floats(&self, plugin: PluginId) -> Vec<FloatSnapshot> {
        let plugin_owner = plugin_owner_key(plugin);
        self.editor
            .model
            .floats
            .iter()
            .filter(|(_, entry)| entry.owner.as_deref() == Some(plugin_owner.as_str()))
            .map(|(id, entry)| FloatSnapshot {
                handle: adapt::float_handle(id),
                title: entry.title.clone(),
                area: AreaSnapshot {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                }, // Area computed at render time
                is_focused: self.editor.model.focus == helix_view::model::FocusTarget::Float(id),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Workspace detail bridge
// ---------------------------------------------------------------------------

impl PluginWorkspaceQueryHost for EditorQueryBridge<'_> {
    fn workspace_detail(&self) -> WorkspaceDetailSnapshot {
        let base = adapt::workspace_snapshot(self.editor);
        let splits = adapt::split_tree_snapshot(self.editor);

        let panels: Vec<PanelSnapshot> = self
            .editor
            .model
            .panels
            .iter()
            .map(|(id, panel)| PanelSnapshot {
                handle: adapt::panel_handle(id),
                title: panel.title.clone(),
                side: adapt::panel_side_to_contract(panel.side),
                visible: panel.visible,
                is_focused: self.editor.model.focus == helix_view::model::FocusTarget::Panel(id),
            })
            .collect();

        let floats: Vec<FloatSnapshot> = self
            .editor
            .model
            .floats
            .iter()
            .map(|(id, entry)| FloatSnapshot {
                handle: adapt::float_handle(id),
                title: entry.title.clone(),
                area: AreaSnapshot {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                is_focused: self.editor.model.focus == helix_view::model::FocusTarget::Float(id),
            })
            .collect();

        let focus = match self.editor.model.focus {
            helix_view::model::FocusTarget::Editor => FocusTargetSnapshot::Editor,
            helix_view::model::FocusTarget::Panel(id) => {
                FocusTargetSnapshot::Panel(adapt::panel_handle(id))
            }
            helix_view::model::FocusTarget::Float(id) => {
                FocusTargetSnapshot::Float(adapt::float_handle(id))
            }
            helix_view::model::FocusTarget::Layer(_) => FocusTargetSnapshot::Layer,
        };

        WorkspaceDetailSnapshot {
            focused_document: base.focused_document,
            focused_view: base.focused_view,
            documents: base.documents,
            views: base.views,
            mode: base.mode,
            splits,
            panels,
            floats,
            focus,
        }
    }
}

// ---------------------------------------------------------------------------
// Assistant query bridge
// ---------------------------------------------------------------------------

impl PluginAssistantQueryHost for EditorQueryBridge<'_> {
    fn assistant_snapshot(&self) -> AssistantSnapshot {
        adapt::assistant_snapshot(self.editor)
    }

    fn thread_snapshot(&self, thread: ThreadHandle) -> ContractResult<AssistantThreadSnapshot> {
        let id = adapt::resolve_thread(thread);
        let thread = self
            .editor
            .assistant
            .thread(id)
            .ok_or_else(|| ContractError::not_found("assistant thread"))?;
        let active = self.editor.assistant.active();
        Ok(adapt::assistant_thread_snapshot(thread, active == Some(id)))
    }

    fn thread_entries(&self, thread: ThreadHandle) -> ContractResult<Vec<AssistantEntrySnapshot>> {
        let id = adapt::resolve_thread(thread);
        let thread = self
            .editor
            .assistant
            .thread(id)
            .ok_or_else(|| ContractError::not_found("assistant thread"))?;
        Ok(adapt::assistant_entries_snapshot(thread))
    }

    fn thread_context(
        &self,
        thread: ThreadHandle,
    ) -> ContractResult<Vec<AssistantContextSnapshot>> {
        let id = adapt::resolve_thread(thread);
        let thread = self
            .editor
            .assistant
            .thread(id)
            .ok_or_else(|| ContractError::not_found("assistant thread"))?;
        Ok(adapt::assistant_context_snapshot(thread))
    }
}

impl PluginAssistantMutationHost for EditorMutationBridge<'_> {
    fn submit_prompt(&mut self, thread: Option<ThreadHandle>, text: String) -> ContractResult<()> {
        let id = resolve_thread_or_active(self.editor, thread)?;
        let effects = self.editor.submit_assistant_prompt(id, text);
        self.editor.apply_assistant_effects(effects);
        Ok(())
    }

    fn cancel_thread(&mut self, thread: Option<ThreadHandle>) -> ContractResult<()> {
        let id = resolve_thread_or_active(self.editor, thread)?;
        let effects = self.editor.cancel_assistant_thread(id);
        self.editor.apply_assistant_effects(effects);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a thread ID or fall back to the active thread.
fn resolve_thread_or_active(
    editor: &Editor,
    thread: Option<ThreadHandle>,
) -> ContractResult<helix_view::assistant::thread::Id> {
    match thread {
        Some(handle) => Ok(adapt::resolve_thread(handle)),
        None => editor
            .assistant
            .active()
            .ok_or_else(|| ContractError::not_found("no active assistant thread")),
    }
}

/// Shared implementation for listing tabs in a view. Used by both the query
/// bridge and the `PluginTabHost` trait impl.
fn visible_view_for_document(
    editor: &Editor,
    doc_id: helix_view::DocumentId,
) -> Option<helix_view::ViewId> {
    editor
        .tree
        .try_get(editor.tree.focus)
        .filter(|view| view.doc == doc_id)
        .map(|view| view.id)
        .or_else(|| {
            editor
                .tree
                .views()
                .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        })
}

fn resolve_operation_view_for_document(
    editor: &mut Editor,
    doc_id: helix_view::DocumentId,
) -> ContractResult<helix_view::ViewId> {
    let view_id = visible_view_for_document(editor, doc_id)
        .or_else(|| editor.tree.views().next().map(|(view, _)| view.id))
        .ok_or_else(|| ContractError::not_found("view"))?;

    let doc = editor
        .document_mut(doc_id)
        .ok_or_else(|| ContractError::not_found("document"))?;
    doc.ensure_view_init(view_id);
    Ok(view_id)
}

/// Resolve an optional `ViewHandle` to a concrete view ID, defaulting to
/// the currently focused view when `None`.
fn resolve_view_or_focused(
    editor: &Editor,
    view: Option<ViewHandle>,
) -> ContractResult<helix_view::ViewId> {
    match view {
        Some(vh) => adapt::resolve_view(editor, vh),
        None => Ok(editor.tree.focus),
    }
}

fn with_view_scope<T>(
    editor: &mut Editor,
    view_id: helix_view::ViewId,
    restore_focus: bool,
    f: impl FnOnce(&mut Editor) -> ContractResult<T>,
) -> ContractResult<T> {
    if restore_focus && view_id != editor.tree.focus {
        editor.with_temporary_focus(view_id, f)
    } else {
        if view_id != editor.tree.focus {
            editor.tree.focus = view_id;
        }
        f(editor)
    }
}

/// Look up the document at `index` in a view's tab list.
///
/// Index 0 is the current document; indices 1+ walk the access history in
/// reverse (most-recent first), matching the ordering used by
/// [`list_tabs_impl`] so indices returned by listing round-trip correctly.
fn tab_doc_at_index(
    editor: &Editor,
    view_id: helix_view::ViewId,
    index: usize,
) -> ContractResult<helix_view::DocumentId> {
    let view = editor.tree.get(view_id);
    if index == 0 {
        return Ok(view.doc);
    }
    // Walk access history in reverse, skipping the current document (which
    // occupies index 0) to mirror the ordering in `list_tabs_impl`.
    let mut remaining = index;
    for &doc_id in view.docs_access_history.iter().rev() {
        if doc_id == view.doc {
            continue;
        }
        remaining -= 1;
        if remaining == 0 {
            return Ok(doc_id);
        }
    }
    Err(ContractError::invalid_request(format!(
        "tab index out of range: {index}"
    )))
}

fn list_tabs_impl(editor: &Editor, view: Option<ViewHandle>) -> ContractResult<TabGroupSnapshot> {
    let view_id = match view {
        Some(vh) => adapt::resolve_view(editor, vh)?,
        None => editor.tree.focus,
    };
    let v = editor.tree.get(view_id);
    let vh = adapt::view_handle(view_id);

    // Build tab list from the view's current doc + access history.
    let mut tabs = Vec::new();
    let current_doc = v.doc;

    // Current document is always the active tab.
    if let Some(doc) = editor.documents.get(&current_doc) {
        tabs.push(TabSnapshot {
            document: adapt::document_handle(current_doc),
            title: doc
                .path()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "[scratch]".into()),
            is_modified: doc.is_modified(),
        });
    }

    // Add recent documents from the access history (excluding current).
    for &doc_id in v.docs_access_history.iter().rev() {
        if doc_id == current_doc {
            continue;
        }
        if let Some(doc) = editor.documents.get(&doc_id) {
            tabs.push(TabSnapshot {
                document: adapt::document_handle(doc_id),
                title: doc
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "[scratch]".into()),
                is_modified: doc.is_modified(),
            });
        }
    }

    Ok(TabGroupSnapshot {
        view: vh,
        tabs,
        active: 0, // Current doc is always index 0
    })
}

/// Convert a contract `FloatPlacement` to a model `Placement`.
fn contract_to_model_placement(
    editor: &Editor,
    fp: &FloatPlacement,
) -> ContractResult<helix_view::model::Placement> {
    match fp {
        FloatPlacement::Centered { width, height } => Ok(helix_view::model::Placement::Centered {
            width: *width,
            height: *height,
        }),
        FloatPlacement::Absolute {
            x,
            y,
            width,
            height,
        } => Ok(helix_view::model::Placement::Float {
            x: *x,
            y: *y,
            width: *width,
            height: *height,
        }),
        FloatPlacement::Anchored {
            view,
            line,
            column,
            width,
            height,
            prefer,
        } => Ok(helix_view::model::Placement::Anchored {
            view: resolve_view_or_focused(editor, *view)?,
            anchor: helix_core::Position {
                row: *line,
                col: *column,
            },
            width: *width,
            height: *height,
            bias: contract_to_anchor_bias(*prefer),
        }),
    }
}

fn contract_to_anchor_bias(prefer: AnchorPreference) -> helix_view::layout::AnchorBias {
    match prefer {
        AnchorPreference::Below => helix_view::layout::AnchorBias::Below,
        AnchorPreference::Above => helix_view::layout::AnchorBias::Above,
    }
}

fn plugin_owner_key(plugin: PluginId) -> String {
    plugin.raw().get().to_string()
}

fn contract_to_model_content(
    editor: &Editor,
    content: FloatContent,
) -> ContractResult<Box<dyn helix_view::model::ContentModel>> {
    use helix_view::model::{DocumentFloatModel, RenderBlock, TextFloatModel};

    Ok(match content {
        FloatContent::Blocks(blocks) => {
            let blocks = blocks
                .into_iter()
                .map(|b| RenderBlock {
                    text: b.text,
                    style: b.style.map(|scope| editor.theme.get(&scope)),
                })
                .collect::<Vec<_>>()
                .into();
            Box::new(TextFloatModel { blocks })
        }
        FloatContent::Document(handle) => Box::new(DocumentFloatModel {
            document: adapt::resolve_document(editor, handle)?,
        }),
    })
}

/// Convert a contract `SplitDirection` to a tree `Direction`.
fn contract_to_tree_direction(dir: SplitDirection) -> helix_view::tree::Direction {
    match dir {
        SplitDirection::Up => helix_view::tree::Direction::Up,
        SplitDirection::Down => helix_view::tree::Direction::Down,
        SplitDirection::Left => helix_view::tree::Direction::Left,
        SplitDirection::Right => helix_view::tree::Direction::Right,
    }
}

/// Convert a contract `Position` (0-based line + column) to a char index in
/// the rope. Returns an error if the position is out of bounds.
fn position_to_char(text: &helix_core::Rope, pos: &Position) -> ContractResult<usize> {
    if pos.line >= text.len_lines() {
        return Err(ContractError::invalid_request(format!(
            "line {} out of range (document has {} lines)",
            pos.line,
            text.len_lines()
        )));
    }
    let line_start = text.line_to_char(pos.line);
    let line_len = text.line(pos.line).len_chars();
    let column = pos.column.min(line_len);
    Ok(line_start + column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::syntax;
    use helix_runtime::Runtime;
    use helix_view::{
        editor::{Action, Config},
        graphics::Rect,
        handlers::Handlers,
        theme,
    };

    #[test]
    fn position_to_char_basic() {
        let text = helix_core::Rope::from("hello\nworld\n");
        // line 0, col 0 → char 0
        assert_eq!(
            position_to_char(&text, &Position { line: 0, column: 0 }).unwrap(),
            0
        );
        // line 0, col 5 → char 5
        assert_eq!(
            position_to_char(&text, &Position { line: 0, column: 5 }).unwrap(),
            5
        );
        // line 1, col 0 → char 6
        assert_eq!(
            position_to_char(&text, &Position { line: 1, column: 0 }).unwrap(),
            6
        );
        // line 1, col 3 → char 9
        assert_eq!(
            position_to_char(&text, &Position { line: 1, column: 3 }).unwrap(),
            9
        );
    }

    #[test]
    fn position_to_char_clamps_column() {
        let text = helix_core::Rope::from("hi\n");
        // col 100 on a 3-char line (including newline) → clamps to 3
        let result = position_to_char(
            &text,
            &Position {
                line: 0,
                column: 100,
            },
        )
        .unwrap();
        assert_eq!(result, 3); // "hi\n" has 3 chars
    }

    #[test]
    fn position_to_char_out_of_range_line() {
        let text = helix_core::Rope::from("hello\n");
        let err = position_to_char(
            &text,
            &Position {
                line: 99,
                column: 0,
            },
        );
        assert!(err.is_err());
    }

    fn test_editor() -> Editor {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syn_loader = Arc::new(ArcSwap::from_pointee(syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let tokio = Box::leak(Box::new(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("tokio runtime"),
        ));
        let _guard = tokio.enter();
        let runtime = Runtime::new(tokio.handle().clone());

        Editor::new(
            Rect::new(0, 0, 120, 40),
            theme_loader,
            syn_loader,
            config,
            runtime,
            Handlers::dummy(),
        )
    }

    fn open_scratch(editor: &mut Editor, action: Action, text: &str) -> helix_view::DocumentId {
        editor.open_markdown_scratch(action, text.to_owned())
    }

    fn document_text(editor: &Editor, doc_id: helix_view::DocumentId) -> String {
        editor
            .document(doc_id)
            .expect("document")
            .text()
            .to_string()
    }

    #[test]
    fn apply_edit_targets_requested_document_when_another_view_is_focused() {
        let mut editor = test_editor();
        let doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .apply_edit(ApplyEditRequest {
                document: adapt::document_handle(doc_two),
                edits: vec![TextEdit {
                    start: Position { line: 0, column: 0 },
                    end: Position { line: 0, column: 0 },
                    new_text: "X".into(),
                }],
            })
            .expect("apply edit");

        assert_eq!(document_text(&editor, doc_one), "one");
        assert_eq!(document_text(&editor, doc_two), "Xtwo");
        assert_eq!(editor.tree.focus, view_one);
        assert_eq!(editor.tree.get(view_two).doc, doc_two);
    }

    #[test]
    fn set_selection_targets_requested_document_view_when_another_view_is_focused() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .set_selection(SetSelectionRequest {
                document: adapt::document_handle(doc_two),
                view: None,
                selections: vec![SelectionRange {
                    anchor: Position { line: 0, column: 0 },
                    head: Position { line: 0, column: 3 },
                }],
            })
            .expect("set selection");

        let selection = editor
            .document(doc_two)
            .unwrap()
            .selection(view_two)
            .primary();
        assert_eq!(selection.from(), 0);
        assert_eq!(selection.to(), 3);
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn document_snapshot_uses_view_showing_requested_document() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        editor.focus(view_one);

        {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            bridge
                .set_selection(SetSelectionRequest {
                    document: adapt::document_handle(doc_two),
                    view: None,
                    selections: vec![SelectionRange {
                        anchor: Position { line: 0, column: 0 },
                        head: Position { line: 0, column: 3 },
                    }],
                })
                .expect("set selection");
        }

        let snapshot = EditorQueryBridge::new(&editor)
            .document_snapshot(adapt::document_handle(doc_two))
            .expect("document snapshot");

        assert_eq!(
            snapshot.selections,
            vec![SelectionRange {
                anchor: Position { line: 0, column: 0 },
                head: Position { line: 0, column: 3 },
            }]
        );
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn set_selection_rejects_view_that_does_not_show_document() {
        let mut editor = test_editor();
        let doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");

        let mut bridge = EditorMutationBridge::new(&mut editor);
        let err = bridge
            .set_selection(SetSelectionRequest {
                document: adapt::document_handle(doc_two),
                view: Some(adapt::view_handle(view_one)),
                selections: vec![SelectionRange {
                    anchor: Position { line: 0, column: 0 },
                    head: Position { line: 0, column: 3 },
                }],
            })
            .expect_err("mismatched view should fail");

        assert!(matches!(err, ContractError::InvalidRequest { .. }));
        assert_eq!(editor.tree.get(view_one).doc, doc_one);
    }

    #[test]
    fn undo_redo_target_requested_document_when_another_view_is_focused() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;

        {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            bridge
                .apply_edit(ApplyEditRequest {
                    document: adapt::document_handle(doc_two),
                    edits: vec![TextEdit {
                        start: Position { line: 0, column: 0 },
                        end: Position { line: 0, column: 0 },
                        new_text: "X".into(),
                    }],
                })
                .expect("apply edit");
        }

        editor.focus(view_one);

        {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            assert!(bridge
                .undo(UndoRequest {
                    document: adapt::document_handle(doc_two),
                })
                .expect("undo"));
        }
        assert_eq!(document_text(&editor, doc_two), "two");
        assert_eq!(editor.tree.focus, view_one);
        assert_eq!(editor.tree.get(view_two).doc, doc_two);

        {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            assert!(bridge
                .redo(RedoRequest {
                    document: adapt::document_handle(doc_two),
                })
                .expect("redo"));
        }
        assert_eq!(document_text(&editor, doc_two), "Xtwo");
        assert_eq!(editor.tree.focus, view_one);
        assert_eq!(editor.tree.get(view_two).doc, doc_two);
    }

    #[test]
    fn split_view_honors_requested_view_without_stealing_focus() {
        let mut editor = test_editor();
        let doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);

        let mut bridge = EditorMutationBridge::new(&mut editor);
        let new_view = bridge
            .split_view(SplitViewRequest {
                view: Some(adapt::view_handle(view_two)),
                direction: SplitDirection::Right,
                document: None,
            })
            .expect("split view");

        let new_view_id = adapt::resolve_view(&editor, new_view).expect("new view handle");
        assert_eq!(editor.tree.get(new_view_id).doc, doc_two);
        assert_eq!(editor.tree.focus, view_one);
        assert_eq!(editor.tree.get(view_one).doc, doc_one);
    }

    #[test]
    fn resize_split_honors_requested_view() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let _doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);

        let before_one = editor.tree.get(view_one).area.width;
        let before_two = editor.tree.get(view_two).area.width;

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .resize_split(ResizeSplitRequest {
                view: Some(adapt::view_handle(view_two)),
                dimension: ResizeDimension::Width,
                amount: ResizeAmount::Grow(1),
            })
            .expect("resize split");

        let after_one = editor.tree.get(view_one).area.width;
        let after_two = editor.tree.get(view_two).area.width;
        assert!(after_one < before_one);
        assert!(after_two > before_two);
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn open_tab_focus_false_adds_tab_to_requested_view() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);
        let doc_three = open_scratch(&mut editor, Action::Load, "three");

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .open_tab(OpenTabRequest {
                view: Some(adapt::view_handle(view_two)),
                document: adapt::document_handle(doc_three),
                focus: false,
            })
            .expect("open tab");

        let tabs = list_tabs_impl(&editor, Some(adapt::view_handle(view_two))).expect("list tabs");
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(tabs.tabs[0].document, adapt::document_handle(doc_two));
        assert_eq!(tabs.tabs[1].document, adapt::document_handle(doc_three));
        assert_eq!(editor.tree.get(view_two).doc, doc_two);
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn cycle_tab_honors_requested_view_without_stealing_focus() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let _doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);
        let doc_three = open_scratch(&mut editor, Action::Load, "three");

        {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            bridge
                .open_tab(OpenTabRequest {
                    view: Some(adapt::view_handle(view_two)),
                    document: adapt::document_handle(doc_three),
                    focus: false,
                })
                .expect("open tab");
        }

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .cycle_tab(CycleTabRequest {
                view: Some(adapt::view_handle(view_two)),
                direction: TabCycleDirection::Next,
            })
            .expect("cycle tab");

        assert_eq!(editor.tree.get(view_two).doc, doc_three);
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn create_float_preserves_anchored_placement_for_requested_view() {
        let mut editor = test_editor();
        let _doc_one = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view_one = editor.tree.focus;
        let _doc_two = open_scratch(&mut editor, Action::VerticalSplit, "two");
        let view_two = editor.tree.focus;
        editor.focus(view_one);

        let mut bridge = EditorMutationBridge::new(&mut editor);
        let handle = bridge
            .create_float(
                PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                CreateFloatRequest {
                    title: Some("hint".into()),
                    placement: FloatPlacement::Anchored {
                        view: Some(adapt::view_handle(view_two)),
                        line: 2,
                        column: 4,
                        width: 30,
                        height: 6,
                        prefer: AnchorPreference::Above,
                    },
                    content: FloatContent::Blocks(Vec::new()),
                    dismissible: true,
                    focus: false,
                },
            )
            .expect("create float");

        let float_id = adapt::resolve_float(&editor.model, handle).expect("float handle");
        assert_eq!(
            editor.model.float(float_id).expect("float").placement,
            helix_view::model::Placement::Anchored {
                view: view_two,
                anchor: helix_core::Position { row: 2, col: 4 },
                width: 30,
                height: 6,
                bias: helix_view::layout::AnchorBias::Above,
            }
        );
        assert_eq!(editor.tree.focus, view_one);
    }

    #[test]
    fn update_float_preserves_anchored_placement() {
        let mut editor = test_editor();
        let _doc = open_scratch(&mut editor, Action::VerticalSplit, "one");
        let view = editor.tree.focus;

        let handle = {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            bridge
                .create_float(
                    PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                    CreateFloatRequest {
                        title: None,
                        placement: FloatPlacement::Centered {
                            width: 20,
                            height: 5,
                        },
                        content: FloatContent::Blocks(Vec::new()),
                        dismissible: true,
                        focus: false,
                    },
                )
                .expect("create float")
        };

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .update_float(
                PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                UpdateFloatRequest {
                    float: handle,
                    title: None,
                    placement: Some(FloatPlacement::Anchored {
                        view: None,
                        line: 3,
                        column: 7,
                        width: 24,
                        height: 8,
                        prefer: AnchorPreference::Below,
                    }),
                    content: None,
                },
            )
            .expect("update float");

        let float_id = adapt::resolve_float(&editor.model, handle).expect("float handle");
        assert_eq!(
            editor.model.float(float_id).expect("float").placement,
            helix_view::model::Placement::Anchored {
                view,
                anchor: helix_core::Position { row: 3, col: 7 },
                width: 24,
                height: 8,
                bias: helix_view::layout::AnchorBias::Below,
            }
        );
    }

    #[test]
    fn update_float_replaces_content_blocks() {
        let mut editor = test_editor();
        let _doc = open_scratch(&mut editor, Action::VerticalSplit, "one");

        let handle = {
            let mut bridge = EditorMutationBridge::new(&mut editor);
            bridge
                .create_float(
                    PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                    CreateFloatRequest {
                        title: None,
                        placement: FloatPlacement::Centered {
                            width: 20,
                            height: 5,
                        },
                        content: FloatContent::Blocks(vec![FloatBlock {
                            text: "old".into(),
                            style: None,
                        }]),
                        dismissible: true,
                        focus: false,
                    },
                )
                .expect("create float")
        };

        let mut bridge = EditorMutationBridge::new(&mut editor);
        bridge
            .update_float(
                PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                UpdateFloatRequest {
                    float: handle,
                    title: None,
                    placement: None,
                    content: Some(FloatContent::Blocks(vec![FloatBlock {
                        text: "new".into(),
                        style: None,
                    }])),
                },
            )
            .expect("update float");

        let float_id = adapt::resolve_float(&editor.model, handle).expect("float handle");
        let model = editor
            .model
            .float(float_id)
            .expect("float")
            .content
            .downcast_ref::<helix_view::model::TextFloatModel>()
            .expect("text float model");
        assert_eq!(model.blocks.len(), 1);
        assert_eq!(model.blocks[0].text, "new");
    }

    #[test]
    fn create_float_preserves_document_content_model() {
        let mut editor = test_editor();
        let doc = open_scratch(&mut editor, Action::VerticalSplit, "document body");

        let mut bridge = EditorMutationBridge::new(&mut editor);
        let handle = bridge
            .create_float(
                PluginId::from_raw(std::num::NonZeroU64::new(1).unwrap()),
                CreateFloatRequest {
                    title: None,
                    placement: FloatPlacement::Centered {
                        width: 20,
                        height: 5,
                    },
                    content: FloatContent::Document(adapt::document_handle(doc)),
                    dismissible: true,
                    focus: false,
                },
            )
            .expect("create float");

        let float_id = adapt::resolve_float(&editor.model, handle).expect("float handle");
        let model = editor
            .model
            .float(float_id)
            .expect("float")
            .content
            .downcast_ref::<helix_view::model::DocumentFloatModel>()
            .expect("document float model");
        assert_eq!(model.document, doc);
    }
}
