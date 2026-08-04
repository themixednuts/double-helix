use crate::{
    AnchorAffinity, ParticipantId, TextAnchor, MAX_BUFFER_SNAPSHOT_BYTES,
    MAX_COLLABORATIVE_FILE_BYTES, MAX_SYNC_MESSAGE_BYTES,
};
use automerge::{
    sync::{Message, State, SyncDoc},
    transaction::Transactable,
    ActorId, AutoCommit, Cursor, LoadOptions, MoveCursor, ObjId, ObjType, Patch, PatchAction,
    PatchLog, ReadDoc, TextEncoding, Value, ROOT,
};
use std::{collections::HashMap, ops::Range};

const TEXT_KEY: &str = "text";

pub struct Buffer {
    doc: AutoCommit,
    text: ObjId,
    materialized: String,
    actor: ParticipantId,
    epoch: u64,
    peers: HashMap<ParticipantId, State>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChange {
    pub start: usize,
    pub end: usize,
    pub insert: String,
}

impl Buffer {
    pub fn new(actor: ParticipantId, content: &str) -> Result<Self, BufferError> {
        if content.len() > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(BufferError::ContentTooLarge);
        }
        let mut doc = AutoCommit::new_with_encoding(TextEncoding::UnicodeCodePoint)
            .with_actor(actor_id(actor));
        let text = doc.put_object(ROOT, TEXT_KEY, ObjType::Text)?;
        if !content.is_empty() {
            doc.splice_text(&text, 0, 0, content)?;
        }
        doc.commit();
        Ok(Self {
            doc,
            text,
            materialized: content.to_owned(),
            actor,
            epoch: 1,
            peers: HashMap::new(),
        })
    }

    pub fn from_snapshot(
        actor: ParticipantId,
        epoch: u64,
        snapshot: &[u8],
    ) -> Result<Self, BufferError> {
        if snapshot.len() > MAX_BUFFER_SNAPSHOT_BYTES {
            return Err(BufferError::SnapshotTooLarge);
        }
        let mut doc = AutoCommit::load_with_options(
            snapshot,
            LoadOptions::new().text_encoding(TextEncoding::UnicodeCodePoint),
        )?;
        doc.set_actor(actor_id(actor));
        let Some((Value::Object(ObjType::Text), text)) = doc.get(ROOT, TEXT_KEY)? else {
            return Err(BufferError::MissingText);
        };
        let materialized = doc.text(&text)?;
        if materialized.len() > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(BufferError::ContentTooLarge);
        }
        Ok(Self {
            doc,
            text,
            materialized,
            actor,
            epoch,
            peers: HashMap::new(),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn text(&self) -> &str {
        &self.materialized
    }

    pub fn anchor(
        &self,
        position: usize,
        affinity: AnchorAffinity,
    ) -> Result<TextAnchor, BufferError> {
        let len = self.doc.length(&self.text);
        if position > len {
            return Err(BufferError::InvalidAnchor { position, len });
        }
        let movement = match affinity {
            AnchorAffinity::Before => MoveCursor::Before,
            AnchorAffinity::After => MoveCursor::After,
        };
        let cursor = self
            .doc
            .get_cursor_moving(&self.text, position, None, movement)?;
        Ok(TextAnchor(cursor.to_bytes().into()))
    }

    pub fn resolve_anchor(&self, anchor: &TextAnchor) -> Result<usize, BufferError> {
        let cursor = Cursor::try_from(anchor.0.as_ref())?;
        self.doc
            .get_cursor_position(&self.text, &cursor, None)
            .map_err(Into::into)
    }

    pub fn replace(&mut self, range: Range<usize>, insert: &str) -> Result<(), BufferError> {
        let len = self.doc.length(&self.text);
        if range.start > range.end || range.end > len {
            return Err(BufferError::InvalidRange {
                start: range.start,
                end: range.end,
                len,
            });
        }
        let byte_range = char_range_to_bytes(&self.materialized, range.clone()).ok_or(
            BufferError::InvalidRange {
                start: range.start,
                end: range.end,
                len,
            },
        )?;
        let projected = self
            .materialized
            .len()
            .saturating_sub(byte_range.len())
            .saturating_add(insert.len());
        if projected > MAX_COLLABORATIVE_FILE_BYTES {
            return Err(BufferError::ContentTooLarge);
        }
        let delete = (range.end - range.start)
            .try_into()
            .map_err(|_| BufferError::EditTooLarge)?;
        self.doc
            .splice_text(&self.text, range.start, delete, insert)?;
        self.doc.commit();
        self.materialized.replace_range(byte_range, insert);
        Ok(())
    }

    pub fn snapshot(&mut self) -> Result<Vec<u8>, BufferError> {
        self.snapshot_for_transfer().map(|(snapshot, _)| snapshot)
    }

    pub(crate) fn snapshot_for_transfer(&mut self) -> Result<(Vec<u8>, bool), BufferError> {
        self.snapshot_with_limit(MAX_BUFFER_SNAPSHOT_BYTES)
    }

    fn snapshot_with_limit(&mut self, limit: usize) -> Result<(Vec<u8>, bool), BufferError> {
        let mut bytes = self.doc.save();
        let compacted = bytes.len() > limit;
        if compacted {
            self.restore_materialized()?;
            bytes = self.doc.save();
        }
        if bytes.len() > limit {
            return Err(BufferError::SnapshotTooLarge);
        }
        Ok((bytes, compacted))
    }

    pub fn sync_message(&mut self, peer: ParticipantId) -> Result<Option<Vec<u8>>, BufferError> {
        let state = self.peers.entry(peer).or_default();
        let Some(message) = self.doc.sync().generate_sync_message(state) else {
            return Ok(None);
        };
        let bytes = message.encode();
        if bytes.len() > MAX_SYNC_MESSAGE_BYTES {
            return Err(BufferError::SyncMessageTooLarge);
        }
        Ok(Some(bytes))
    }

    pub fn receive_sync(
        &mut self,
        peer: ParticipantId,
        epoch: u64,
        bytes: &[u8],
    ) -> Result<Vec<TextChange>, BufferError> {
        if epoch != self.epoch {
            return Err(BufferError::EpochMismatch {
                expected: self.epoch,
                actual: epoch,
            });
        }
        if bytes.len() > MAX_SYNC_MESSAGE_BYTES {
            return Err(BufferError::SyncMessageTooLarge);
        }
        let message = Message::decode(bytes)?;
        let mut patch_log = PatchLog::active();
        let state = self.peers.entry(peer).or_default();
        self.doc
            .sync()
            .receive_sync_message_log_patches(state, message, &mut patch_log)?;
        let patches = self.doc.make_patches(&mut patch_log);
        let changes = match text_changes(&self.text, patches) {
            Ok(changes) => changes,
            Err(error) => {
                self.restore_materialized()?;
                return Err(error);
            }
        };
        let inserted = changes.iter().fold(0_usize, |bytes, change| {
            bytes.saturating_add(change.insert.len())
        });
        let applied =
            if self.materialized.len().saturating_add(inserted) <= MAX_COLLABORATIVE_FILE_BYTES {
                apply_text_changes(&mut self.materialized, &changes)
            } else {
                let mut projected = self.materialized.clone();
                apply_text_changes(&mut projected, &changes).and_then(|()| {
                    if projected.len() > MAX_COLLABORATIVE_FILE_BYTES {
                        Err(BufferError::ContentTooLarge)
                    } else {
                        self.materialized = projected;
                        Ok(())
                    }
                })
            };
        if let Err(error) = applied {
            self.restore_materialized()?;
            return Err(error);
        }
        Ok(changes)
    }

    pub fn reset_epoch(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.peers.clear();
    }

    fn restore_materialized(&mut self) -> Result<(), BufferError> {
        let epoch = self.epoch.saturating_add(1);
        let mut restored = Self::new(self.actor, &self.materialized)?;
        restored.epoch = epoch;
        *self = restored;
        Ok(())
    }
}

fn actor_id(participant: ParticipantId) -> ActorId {
    ActorId::from(participant.0.to_vec())
}

fn text_changes(text: &ObjId, patches: Vec<Patch>) -> Result<Vec<TextChange>, BufferError> {
    patches
        .into_iter()
        .map(|patch| {
            if &patch.obj != text {
                return Err(BufferError::UnsupportedChange);
            }
            match patch.action {
                PatchAction::SpliceText {
                    index,
                    value,
                    marks: None,
                } => Ok(TextChange {
                    start: index,
                    end: index,
                    insert: value.make_string(),
                }),
                PatchAction::DeleteSeq { index, length } => Ok(TextChange {
                    start: index,
                    end: index.saturating_add(length),
                    insert: String::new(),
                }),
                _ => Err(BufferError::UnsupportedChange),
            }
        })
        .collect()
}

fn apply_text_changes(text: &mut String, changes: &[TextChange]) -> Result<(), BufferError> {
    for change in changes {
        let range = change.start..change.end;
        let Some(bytes) = char_range_to_bytes(text, range.clone()) else {
            return Err(BufferError::InvalidRange {
                start: range.start,
                end: range.end,
                len: text.chars().count(),
            });
        };
        text.replace_range(bytes, &change.insert);
    }
    Ok(())
}

fn char_range_to_bytes(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    if range.start > range.end {
        return None;
    }
    let mut start = None;
    for (index, byte) in text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .enumerate()
    {
        if index == range.start {
            start = Some(byte);
        }
        if index == range.end {
            return start.map(|start| start..byte);
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error(transparent)]
    Automerge(#[from] automerge::AutomergeError),
    #[error(transparent)]
    DecodeSync(#[from] automerge::sync::ReadMessageError),
    #[error("collaboration buffer snapshot is too large")]
    SnapshotTooLarge,
    #[error("collaboration buffer text exceeds the editing size limit")]
    ContentTooLarge,
    #[error("collaboration sync message is too large")]
    SyncMessageTooLarge,
    #[error("collaboration edit is too large for the text engine")]
    EditTooLarge,
    #[error("collaboration buffer snapshot has no text object")]
    MissingText,
    #[error("collaboration sync attempted to mutate unsupported document state")]
    UnsupportedChange,
    #[error("collaboration buffer epoch mismatch: expected {expected}, got {actual}")]
    EpochMismatch { expected: u64, actual: u64 },
    #[error("collaboration edit range {start}..{end} exceeds buffer length {len}")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("collaboration anchor {position} exceeds buffer length {len}")]
    InvalidAnchor { position: usize, len: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(value: u8) -> ParticipantId {
        ParticipantId([value; 16])
    }

    fn exchange(
        left: &mut Buffer,
        left_id: ParticipantId,
        right: &mut Buffer,
        right_id: ParticipantId,
    ) {
        for _ in 0..16 {
            let mut progressed = false;
            if let Some(message) = left.sync_message(right_id).unwrap() {
                right
                    .receive_sync(left_id, right.epoch(), &message)
                    .unwrap();
                progressed = true;
            }
            if let Some(message) = right.sync_message(left_id).unwrap() {
                left.receive_sync(right_id, left.epoch(), &message).unwrap();
                progressed = true;
            }
            if !progressed {
                return;
            }
        }
        panic!("replicas did not converge within the exchange budget");
    }

    #[test]
    fn concurrent_edits_converge() {
        let host = participant(1);
        let left_id = participant(2);
        let right_id = participant(3);
        let mut source = Buffer::new(host, "abc").unwrap();
        let snapshot = source.snapshot().unwrap();
        let mut left = Buffer::from_snapshot(left_id, 1, &snapshot).unwrap();
        let mut right = Buffer::from_snapshot(right_id, 1, &snapshot).unwrap();

        left.replace(1..1, "x").unwrap();
        right.replace(1..1, "y").unwrap();
        exchange(&mut left, left_id, &mut right, right_id);

        assert_eq!(left.text(), right.text());
        assert!(left.text().contains(['x', 'y']));
    }

    #[test]
    fn changes_use_helix_character_offsets() {
        let host = participant(1);
        let guest = participant(2);
        let mut left = Buffer::new(host, "a😀c").unwrap();
        let snapshot = left.snapshot().unwrap();
        let mut right = Buffer::from_snapshot(guest, 1, &snapshot).unwrap();
        exchange(&mut left, host, &mut right, guest);
        right.replace(1..2, "界").unwrap();
        let message = right.sync_message(host).unwrap().unwrap();
        let changes = left.receive_sync(guest, 1, &message).unwrap();
        assert_eq!(
            changes,
            vec![
                TextChange {
                    start: 1,
                    end: 1,
                    insert: "界".to_owned(),
                },
                TextChange {
                    start: 2,
                    end: 3,
                    insert: String::new(),
                },
            ]
        );
        assert_eq!(left.text(), "a界c");
    }

    #[test]
    fn unsupported_crdt_state_is_removed_and_forces_a_new_epoch() {
        let host = participant(1);
        let guest = participant(2);
        let mut left = Buffer::new(host, "safe").unwrap();
        let snapshot = left.snapshot().unwrap();
        let mut right = Buffer::from_snapshot(guest, 1, &snapshot).unwrap();
        exchange(&mut left, host, &mut right, guest);
        right.doc.put(ROOT, "hidden", "payload").unwrap();
        right.doc.commit();
        let message = right.sync_message(host).unwrap().unwrap();

        assert!(matches!(
            left.receive_sync(guest, 1, &message),
            Err(BufferError::UnsupportedChange)
        ));
        assert_eq!(left.text(), "safe");
        assert_eq!(left.epoch(), 2);
        assert!(left.doc.get(ROOT, "hidden").unwrap().is_none());
    }

    #[test]
    fn oversized_history_is_compacted_without_changing_live_text() {
        let host = participant(1);
        let guest = participant(2);
        let mut buffer = Buffer::new(host, "😀").unwrap();
        buffer.peers.insert(guest, State::new());
        for index in 0..512 {
            buffer
                .replace(0..1, if index % 2 == 0 { "界" } else { "😀" })
                .unwrap();
        }
        let expected = buffer.text().to_owned();
        let history_bytes = buffer.doc.save().len();
        let compact_bytes = Buffer::new(host, &expected)
            .unwrap()
            .snapshot()
            .unwrap()
            .len();
        assert!(history_bytes > compact_bytes);
        let limit = compact_bytes + (history_bytes - compact_bytes) / 2;

        let (snapshot, compacted) = buffer.snapshot_with_limit(limit).unwrap();

        assert!(compacted);
        assert!(snapshot.len() <= limit);
        assert_eq!(buffer.text(), expected);
        assert_eq!(buffer.epoch(), 2);
        assert!(buffer.peers.is_empty());
        let restored = Buffer::from_snapshot(guest, buffer.epoch(), &snapshot).unwrap();
        assert_eq!(restored.text(), expected);
    }

    #[test]
    fn anchors_track_concurrent_insertions() {
        let left_id = participant(1);
        let right_id = participant(2);
        let mut left = Buffer::new(left_id, "abc").unwrap();
        let snapshot = left.snapshot().unwrap();
        let mut right = Buffer::from_snapshot(right_id, 1, &snapshot).unwrap();
        let anchor = left.anchor(2, AnchorAffinity::After).unwrap();

        right.replace(0..0, "z").unwrap();
        exchange(&mut left, left_id, &mut right, right_id);

        assert_eq!(left.resolve_anchor(&anchor).unwrap(), 3);
    }
}
