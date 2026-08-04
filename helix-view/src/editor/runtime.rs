use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use anyhow::bail;
use futures_util::stream::SelectAll;
use helix_dap as dap;
use helix_lsp::{LanguageServerId, ServerEvent};
use helix_runtime::{FrameHandle, FrameReceiver, Receiver as RuntimeReceiver, Runtime, Work};

use crate::document::{DocumentSavedEvent, DocumentSavedEventResult};
use crate::file_bound::DocumentLocation;
use crate::DocumentId;

use super::{ConfigEvent, Editor};

#[derive(Debug, Clone)]
pub struct DocumentSaveReport {
    pub doc_id: DocumentId,
    pub location: DocumentLocation,
    pub line_count: usize,
    pub byte_count: usize,
}

impl Editor {
    pub fn take_config_rx(&mut self) -> RuntimeReceiver<ConfigEvent> {
        std::mem::replace(&mut self.config_events.1, helix_runtime::channel(1).1)
    }

    pub fn take_redraw_rx(&mut self) -> FrameReceiver {
        self.frame_gate.take_receiver()
    }

    pub fn redraw_handle(&self) -> FrameHandle {
        self.frame_gate.handle()
    }

    pub fn take_assistant_updates_rx(
        &mut self,
    ) -> RuntimeReceiver<crate::assistant::backend::Update> {
        std::mem::replace(
            &mut self.assistant_runtime.updates_rx,
            helix_runtime::channel(1).1,
        )
    }

    pub fn take_lsp_incoming(
        &mut self,
    ) -> SelectAll<helix_runtime::Receiver<(LanguageServerId, ServerEvent)>> {
        std::mem::replace(&mut self.language_servers.incoming, SelectAll::new())
    }

    pub fn take_debugger_incoming(
        &mut self,
    ) -> SelectAll<helix_runtime::Receiver<(dap::registry::DebugAdapterId, dap::ServerEvent)>> {
        std::mem::replace(&mut self.debug_adapters.incoming, SelectAll::new())
    }

    pub fn request_redraw(&self) {
        self.frame_gate.request_redraw();
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn work(&self) -> Work {
        self.runtime.work().clone()
    }

    pub fn save(
        &mut self,
        doc_id: DocumentId,
        path: Option<super::WorkspaceDocumentPath>,
        policy: super::SavePolicy,
    ) -> anyhow::Result<()> {
        let remote_backend = self.workspace_backend.remote().cloned();
        let location = self
            .document(doc_id)
            .and_then(|document| document.location())
            .cloned();
        let destination = path.clone().or_else(|| {
            location.as_ref().map(|location| match location {
                DocumentLocation::Local(path) => super::WorkspaceDocumentPath::Local(path.clone()),
                DocumentLocation::Remote(location) => {
                    super::WorkspaceDocumentPath::Remote(location.path.clone())
                }
                DocumentLocation::Collaboration(location) => {
                    super::WorkspaceDocumentPath::Collaboration {
                        project: location.project,
                        path: location.path.clone(),
                    }
                }
            })
        });
        let collaboration = self
            .collaboration
            .buffer(doc_id)
            .map(|buffer| {
                self.collaboration
                    .session()
                    .map(|session| (session, buffer))
                    .ok_or_else(|| anyhow::anyhow!("collaboration session is disconnected"))
            })
            .transpose()?;
        let destination =
            destination.ok_or_else(|| anyhow::anyhow!("Can't save with no path set!"))?;
        let save_lock = self
            .save_locks
            .get(&doc_id)
            .cloned()
            .ok_or_else(|| anyhow::format_err!("save lock is closed for this document!"))?;
        let work = self.work();
        let doc = doc_mut!(self, &doc_id);
        let doc_save_task = match destination {
            super::WorkspaceDocumentPath::Local(destination) => {
                if matches!(
                    location,
                    Some(DocumentLocation::Remote(_) | DocumentLocation::Collaboration(_))
                ) {
                    anyhow::bail!("cannot save a remote or collaborative document to a local path");
                }
                doc.save_serialized(&work, path.map(|_| destination), policy, save_lock)?
            }
            super::WorkspaceDocumentPath::Remote(destination) => {
                if collaboration.is_some() {
                    anyhow::bail!(
                        "hosted remote documents must be saved through the collaboration session"
                    );
                }
                let backend = remote_backend
                    .ok_or_else(|| anyhow::anyhow!("remote workspace is disconnected"))?;
                doc.save_remote_serialized(
                    &work,
                    backend,
                    path.map(|_| destination),
                    policy,
                    save_lock,
                )?
            }
            super::WorkspaceDocumentPath::Collaboration {
                project,
                path: destination,
            } => {
                if path.is_some()
                    && !matches!(
                        location,
                        Some(DocumentLocation::Collaboration(ref location))
                            if location.project == project && location.path == destination
                    )
                {
                    anyhow::bail!("save-as is not supported for collaborative documents");
                }
                let (session, buffer) = collaboration
                    .ok_or_else(|| anyhow::anyhow!("collaboration document is not connected"))?;
                doc.save_collaboration_serialized(&work, session, buffer, policy, save_lock)?
            }
        };

        let handler = self.language_servers.file_event_handler.clone();
        let task = work.spawn(async move {
            let res = match doc_save_task.await {
                Ok(res) => res,
                Err(err) => return Err(anyhow::anyhow!("document save task failed: {err}")),
            };
            if let Ok(Some(event)) = &res {
                if let Some(path) = event.location.local_path() {
                    handler.file_changed(path.to_path_buf());
                }
            }
            res
        });

        self.save_queue
            .push_back(super::core::PendingDocumentSave { doc_id, task });
        self.write_count += 1;

        Ok(())
    }

    pub fn apply_document_saved_event(
        &mut self,
        save_event: DocumentSavedEvent,
    ) -> Option<DocumentSaveReport> {
        let doc_id = save_event.doc_id;
        let location = save_event.location;
        let line_count = save_event.text.len_lines();
        let byte_count = save_event.text.len_bytes();

        {
            let doc = match self.document_mut(doc_id) {
                None => {
                    log::warn!(
                        "received document saved event for non-existent doc id: {}",
                        doc_id
                    );
                    return None;
                }
                Some(doc) => doc,
            };

            log::debug!(
                "document {:?} saved with revision {}",
                doc.path(),
                save_event.revision
            );

            doc.set_last_saved_revision(save_event.revision, save_event.save_time);
            if matches!(
                location,
                DocumentLocation::Remote(_) | DocumentLocation::Collaboration(_)
            ) {
                doc.apply_saved_location(location.clone());
            }
        }

        if let DocumentLocation::Local(path) = &location {
            self.set_doc_path(doc_id, path);
        }

        Some(DocumentSaveReport {
            doc_id,
            location,
            line_count,
            byte_count,
        })
    }

    /// Wait for the next completed document save.
    ///
    /// Important: the front queue entry is only removed once its task is
    /// `Ready`. Popping earlier is unsafe under `tokio::select!` — if another
    /// branch wins, this future is dropped and the save result (which clears
    /// the `[+]` dirty marker) is lost even though the file was already
    /// written.
    pub async fn recv_save_result(&mut self) -> Option<DocumentSavedEventResult> {
        std::future::poll_fn(|cx| {
            let Some(pending) = self.save_queue.front_mut() else {
                return Poll::Ready(None);
            };
            match Pin::new(&mut pending.task).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(join_result) => {
                    let _ = self.save_queue.pop_front();
                    self.write_count = self.write_count.saturating_sub(1);
                    Poll::Ready(Some(match join_result {
                        Ok(result) => result,
                        Err(err) => Err(anyhow::anyhow!("document save task failed: {err}")),
                    }))
                }
            }
        })
        .await
    }

    pub fn has_pending_writes(&self) -> bool {
        self.write_count > 0
    }

    pub fn pending_write_documents(&self) -> impl Iterator<Item = DocumentId> + '_ {
        self.save_queue.iter().map(|pending| pending.doc_id)
    }

    pub async fn flush_writes(&mut self) -> anyhow::Result<()> {
        while self.write_count > 0 {
            let Some(save_result) = self.recv_save_result().await else {
                break;
            };

            let Some(save_event) = (match save_result {
                Ok(event) => event,
                Err(err) => {
                    self.set_error(err.to_string());
                    bail!(err);
                }
            }) else {
                continue;
            };

            let _ = self.apply_document_saved_event(save_event);
        }

        Ok(())
    }
}
