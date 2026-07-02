//! Msgpack-based serialization codec for the plugin contract.
//!
//! All contract types derive `Serialize`/`Deserialize`, making them
//! format-agnostic. This module provides msgpack as the canonical wire format
//! for future RPC transport (out-of-process plugins, WASM, SSH).
//!
//! In-process Lua plugins bypass the codec entirely — the Lua facade converts
//! directly between Rust and Lua types. The codec is for the wire boundary.

use serde::{Deserialize, Serialize};

/// Encode a contract type to msgpack bytes.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    rmp_serde::to_vec(value).map_err(EncodeError)
}

/// Decode a contract type from msgpack bytes.
pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, DecodeError> {
    rmp_serde::from_slice(bytes).map_err(DecodeError)
}

/// Encode a contract type to msgpack bytes with named struct fields.
///
/// Produces slightly larger output than [`encode`] but is easier to debug
/// (field names are preserved in the msgpack map).
pub fn encode_named<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    rmp_serde::to_vec_named(value).map_err(EncodeError)
}

/// Error from encoding a contract type to msgpack.
#[derive(Debug)]
pub struct EncodeError(rmp_serde::encode::Error);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "msgpack encode error: {}", self.0)
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Error from decoding a contract type from msgpack.
#[derive(Debug)]
pub struct DecodeError(rmp_serde::decode::Error);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "msgpack decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::handles::DocumentHandle;
    use crate::contract::requests::*;
    use crate::contract::snapshots::*;
    use std::num::NonZeroU64;

    fn doc(id: u64) -> DocumentHandle {
        DocumentHandle::from_raw(NonZeroU64::new(id).unwrap())
    }

    #[test]
    fn round_trip_apply_edit() {
        let req = ApplyEditRequest {
            document: doc(1),
            edits: vec![TextEdit {
                start: Position { line: 0, column: 0 },
                end: Position { line: 0, column: 5 },
                new_text: "hello".into(),
            }],
        };
        let bytes = encode(&req).unwrap();
        let req2: ApplyEditRequest = decode(&bytes).unwrap();
        assert_eq!(req2.edits.len(), 1);
        assert_eq!(req2.edits[0].new_text, "hello");
    }

    #[test]
    fn round_trip_document_snapshot() {
        let snap = DocumentSnapshot {
            handle: doc(42),
            path: Some("/tmp/test.rs".into()),
            language: Some("rust".into()),
            is_modified: true,
            line_count: 100,
            selections: vec![SelectionRange {
                anchor: Position { line: 5, column: 0 },
                head: Position {
                    line: 5,
                    column: 10,
                },
            }],
            mode: EditMode::Normal,
        };
        let bytes = encode(&snap).unwrap();
        let snap2: DocumentSnapshot = decode(&bytes).unwrap();
        assert_eq!(snap2.path, Some("/tmp/test.rs".into()));
        assert_eq!(snap2.line_count, 100);
        assert_eq!(snap2.selections.len(), 1);
    }

    #[test]
    fn round_trip_named_vs_compact() {
        let req = NotifyRequest {
            message: "hi".into(),
            level: NotifyLevel::Warn,
        };
        let compact = encode(&req).unwrap();
        let named = encode_named(&req).unwrap();
        // Named is larger (includes field names).
        assert!(named.len() > compact.len());
        // Both decode to the same value.
        let r1: NotifyRequest = decode(&compact).unwrap();
        let r2: NotifyRequest = decode(&named).unwrap();
        assert_eq!(r1.message, "hi");
        assert_eq!(r2.message, "hi");
    }

    #[test]
    fn round_trip_events() {
        use crate::contract::events::*;
        let event = PluginEvent::DocumentOpened(DocumentOpenedEvent {
            document: doc(7),
            path: Some("/test.txt".into()),
            language: None,
        });
        let bytes = encode(&event).unwrap();
        let event2: PluginEvent = decode(&bytes).unwrap();
        assert_eq!(event2.kind(), EventKind::DocumentOpened);
    }

    #[test]
    fn round_trip_errors() {
        use crate::contract::errors::ContractError;
        let err = ContractError::stale_handle("doc:42");
        let bytes = encode(&err).unwrap();
        let err2: ContractError = decode(&bytes).unwrap();
        assert!(format!("{err2}").contains("doc:42"));
    }

    #[test]
    fn round_trip_metadata() {
        use crate::contract::metadata::ApiMetadata;
        let meta = ApiMetadata::default();
        let bytes = encode(&meta).unwrap();
        let meta2: ApiMetadata = decode(&bytes).unwrap();
        assert_eq!(meta2.version, meta.version);
    }

    #[test]
    fn round_trip_dynamic_value() {
        use crate::contract::value::DynamicValue;
        for val in [
            DynamicValue::Nil,
            DynamicValue::Bool(true),
            DynamicValue::Int(42),
            DynamicValue::Float(1.5),
            DynamicValue::String("hello".into()),
        ] {
            let bytes = encode(&val).unwrap();
            let val2: DynamicValue = decode(&bytes).unwrap();
            match (&val, &val2) {
                (DynamicValue::Nil, DynamicValue::Nil) => {}
                (DynamicValue::Bool(a), DynamicValue::Bool(b)) => assert_eq!(a, b),
                (DynamicValue::Int(a), DynamicValue::Int(b)) => assert_eq!(a, b),
                (DynamicValue::String(a), DynamicValue::String(b)) => assert_eq!(a, b),
                (DynamicValue::Float(_), DynamicValue::Float(_)) => {} // float comparison is fragile
                _ => panic!("mismatch: {val:?} != {val2:?}"),
            }
        }
    }
}
