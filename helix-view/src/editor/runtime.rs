use std::path::PathBuf;

use anyhow::bail;
use futures_util::stream::SelectAll;
use helix_dap as dap;
use helix_lsp::{Call, LanguageServerId};
use helix_runtime::{FrameHandle, FrameReceiver, Receiver as RuntimeReceiver, Runtime, Work};

use crate::document::{DocumentSavedEvent, DocumentSavedEventResult};
use crate::DocumentId;

use super::{ConfigEvent, Editor};

#[derive(Debug, Clone)]
pub struct DocumentSaveReport {
    pub doc_id: DocumentId,
    pub path: PathBuf,
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
    ) -> SelectAll<helix_runtime::Receiver<(LanguageServerId, Call)>> {
        std::mem::replace(&mut self.language_servers.incoming, SelectAll::new())
    }

    pub fn take_debugger_incoming(
        &mut self,
    ) -> SelectAll<helix_runtime::Receiver<(dap::registry::DebugAdapterId, dap::Payload)>> {
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

    pub fn save<P: Into<PathBuf>>(
        &mut self,
        doc_id: DocumentId,
        path: Option<P>,
        policy: super::SavePolicy,
    ) -> anyhow::Result<()> {
        let path = path.map(|path| path.into());
        let save_lock = self
            .save_locks
            .get(&doc_id)
            .cloned()
            .ok_or_else(|| anyhow::format_err!("save lock is closed for this document!"))?;
        let work = self.work();
        let doc = doc_mut!(self, &doc_id);
        let doc_save_task = doc.save_serialized(&work, path, policy, save_lock)?;

        let handler = self.language_servers.file_event_handler.clone();
        let task = work.spawn(async move {
            let res = match doc_save_task.await {
                Ok(res) => res,
                Err(err) => return Err(anyhow::anyhow!("document save task failed: {err}")),
            };
            if let Ok(Some(event)) = &res {
                handler.file_changed(event.path.clone());
            }
            res
        });

        self.save_queue.push_back(task);
        self.write_count += 1;

        Ok(())
    }

    pub fn apply_document_saved_event(
        &mut self,
        save_event: DocumentSavedEvent,
    ) -> Option<DocumentSaveReport> {
        let doc_id = save_event.doc_id;
        let path = save_event.path;
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
        }

        self.set_doc_path(doc_id, &path);

        Some(DocumentSaveReport {
            doc_id,
            path,
            line_count,
            byte_count,
        })
    }

    pub async fn recv_save_result(&mut self) -> Option<DocumentSavedEventResult> {
        let save_task = self.save_queue.pop_front()?;
        self.write_count = self.write_count.saturating_sub(1);
        Some(match save_task.await {
            Ok(result) => result,
            Err(err) => Err(anyhow::anyhow!("document save task failed: {err}")),
        })
    }

    pub fn has_pending_writes(&self) -> bool {
        self.write_count > 0
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
