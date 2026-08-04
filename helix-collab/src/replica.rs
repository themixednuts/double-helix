use crate::{
    AnchorAffinity, Buffer, BufferError, BufferId, Event, ParticipantId, ProjectInfo, Request,
    Response, TextAnchor, TextChange,
};
use serde_bytes::ByteBuf;
use std::{collections::HashMap, ops::Range};

/// Client-owned replicated buffers for one hosted project.
pub struct ReplicaProject {
    participant: ParticipantId,
    host: ParticipantId,
    buffers: HashMap<BufferId, Buffer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaUpdate {
    pub buffer: BufferId,
    pub changes: Vec<TextChange>,
    pub reply: Option<Request>,
}

impl ReplicaProject {
    pub fn new(participant: ParticipantId, project: &ProjectInfo) -> Self {
        Self {
            participant,
            host: project.owner,
            buffers: HashMap::new(),
        }
    }

    pub fn participant(&self) -> ParticipantId {
        self.participant
    }

    pub fn contains(&self, buffer: BufferId) -> bool {
        self.buffers.contains_key(&buffer)
    }

    pub fn release(&mut self, buffer: BufferId) -> bool {
        self.buffers.remove(&buffer).is_some()
    }

    pub fn install(&mut self, response: Response) -> Result<BufferId, ReplicaError> {
        let Response::Buffer {
            buffer,
            epoch,
            total_bytes,
            snapshot,
            continuation,
        } = response
        else {
            return Err(ReplicaError::UnexpectedResponse);
        };
        if continuation.is_some() || total_bytes != snapshot.len() as u64 {
            return Err(ReplicaError::IncompleteSnapshot);
        }
        let document = Buffer::from_snapshot(self.participant, epoch, &snapshot)?;
        self.buffers.insert(buffer, document);
        Ok(buffer)
    }

    pub fn text(&self, buffer: BufferId) -> Result<String, ReplicaError> {
        Ok(self.buffer(buffer)?.text().to_owned())
    }

    pub fn replace(
        &mut self,
        buffer: BufferId,
        range: Range<usize>,
        insert: &str,
    ) -> Result<Option<Request>, ReplicaError> {
        self.buffer_mut(buffer)?.replace(range, insert)?;
        self.next_sync(buffer)
    }

    pub fn replace_many(
        &mut self,
        buffer: BufferId,
        changes: &[TextChange],
    ) -> Result<Option<Request>, ReplicaError> {
        self.apply_changes(buffer, changes)?;
        self.next_sync(buffer)
    }

    pub fn replace_batches(
        &mut self,
        buffer: BufferId,
        batches: &[Vec<TextChange>],
    ) -> Result<Option<Request>, ReplicaError> {
        for changes in batches {
            self.apply_changes(buffer, changes)?;
        }
        self.next_sync(buffer)
    }

    pub fn replace_all(
        &mut self,
        buffer: BufferId,
        text: &str,
    ) -> Result<Option<Request>, ReplicaError> {
        let len = self.buffer(buffer)?.text().chars().count();
        self.buffer_mut(buffer)?.replace(0..len, text)?;
        self.next_sync(buffer)
    }

    pub fn sync_all(&mut self) -> Result<Vec<Request>, ReplicaError> {
        let buffers = self.buffers.keys().copied().collect::<Vec<_>>();
        buffers
            .into_iter()
            .filter_map(|buffer| self.next_sync(buffer).transpose())
            .collect()
    }

    pub fn anchor(
        &self,
        buffer: BufferId,
        position: usize,
        affinity: AnchorAffinity,
    ) -> Result<TextAnchor, ReplicaError> {
        self.buffer(buffer)?
            .anchor(position, affinity)
            .map_err(Into::into)
    }

    pub fn resolve_anchor(
        &self,
        buffer: BufferId,
        anchor: &TextAnchor,
    ) -> Result<usize, ReplicaError> {
        self.buffer(buffer)?
            .resolve_anchor(anchor)
            .map_err(Into::into)
    }

    pub fn apply(&mut self, event: Event) -> Result<Option<ReplicaUpdate>, ReplicaError> {
        match event {
            Event::BufferSync {
                buffer,
                epoch,
                message,
            } => {
                let host = self.host;
                let changes = self
                    .buffer_mut(buffer)?
                    .receive_sync(host, epoch, &message)?;
                let reply = self.next_sync(buffer)?;
                Ok(Some(ReplicaUpdate {
                    buffer,
                    changes,
                    reply,
                }))
            }
            Event::ResyncRequired { buffer, .. } => Ok(Some(ReplicaUpdate {
                buffer,
                changes: Vec::new(),
                reply: Some(Request::ReadBuffer { buffer }),
            })),
            Event::ProjectState(_)
            | Event::ParticipantJoined(_)
            | Event::ParticipantLeft { .. }
            | Event::RoleChanged { .. }
            | Event::Presence(_)
            | Event::PresenceCleared { .. }
            | Event::FollowRequested { .. }
            | Event::BufferSaved { .. }
            | Event::FilesChanged { .. }
            | Event::WorktreeChanged { .. }
            | Event::LanguageServerDiagnostics(_)
            | Event::LanguageServerRefresh(_) => Ok(None),
        }
    }

    fn next_sync(&mut self, buffer: BufferId) -> Result<Option<Request>, ReplicaError> {
        let host = self.host;
        let document = self.buffer_mut(buffer)?;
        let epoch = document.epoch();
        Ok(document
            .sync_message(host)?
            .map(|message| Request::SyncBuffer {
                buffer,
                epoch,
                message: ByteBuf::from(message),
            }))
    }

    fn apply_changes(
        &mut self,
        buffer: BufferId,
        changes: &[TextChange],
    ) -> Result<(), ReplicaError> {
        let len = self.buffer(buffer)?.text().chars().count();
        let mut previous_end = 0;
        for change in changes {
            if change.start < previous_end || change.start > change.end || change.end > len {
                return Err(ReplicaError::InvalidChanges);
            }
            previous_end = change.end;
        }
        let document = self.buffer_mut(buffer)?;
        for change in changes.iter().rev() {
            document.replace(change.start..change.end, &change.insert)?;
        }
        Ok(())
    }

    fn buffer(&self, buffer: BufferId) -> Result<&Buffer, ReplicaError> {
        self.buffers
            .get(&buffer)
            .ok_or(ReplicaError::UnknownBuffer(buffer))
    }

    fn buffer_mut(&mut self, buffer: BufferId) -> Result<&mut Buffer, ReplicaError> {
        self.buffers
            .get_mut(&buffer)
            .ok_or(ReplicaError::UnknownBuffer(buffer))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReplicaError {
    #[error(transparent)]
    Buffer(#[from] BufferError),
    #[error("collaboration response did not contain a buffer")]
    UnexpectedResponse,
    #[error("collaboration response contained an incomplete buffer snapshot")]
    IncompleteSnapshot,
    #[error("collaboration buffer {0:?} is not open")]
    UnknownBuffer(BufferId),
    #[error("collaboration changes are invalid or overlap")]
    InvalidChanges,
}
