//! Stdio RPC transport for out-of-process plugin runtimes.
//!
//! Frames use the shared [`helix_ipc::FrameCodec`]. The plugin request and
//! response contract remains owned by this crate.

mod host_runner;
mod protocol;

pub use helix_ipc::{FrameCodec, FrameError, FrameResult};
pub use host_runner::run_plugin_host;
pub use protocol::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::metadata::ApiMetadata;
    use crate::types::PluginConfig;
    use std::io::{Cursor, Read};

    #[test]
    fn round_trip_sync_frame() {
        let value: Frame<HostRequest, PluginResponse> = Frame::Request {
            id: 7,
            body: HostRequest::Init {
                metadata: ApiMetadata::default(),
                config: PluginConfig::default(),
            },
        };
        let mut codec = FrameCodec::new();
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &value).unwrap();

        let mut input = Cursor::new(bytes);
        let decoded: Frame<HostRequest, PluginResponse> = codec.read_sync(&mut input).unwrap();
        assert!(matches!(
            decoded,
            Frame::Request {
                id: 7,
                body: HostRequest::Init { .. }
            }
        ));
    }

    #[test]
    fn partial_reads_are_accumulated_by_read_exact() {
        struct Slow(Cursor<Vec<u8>>);

        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let max = buf.len().min(2);
                self.0.read(&mut buf[..max])
            }
        }

        let value: Frame<HostRequest, PluginResponse> = Frame::Notify {
            body: HostRequest::Shutdown,
        };
        let mut codec = FrameCodec::with_floor(16);
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &value).unwrap();

        let decoded: Frame<HostRequest, PluginResponse> =
            codec.read_sync(&mut Slow(Cursor::new(bytes))).unwrap();
        assert!(matches!(
            decoded,
            Frame::Notify {
                body: HostRequest::Shutdown
            }
        ));
    }

    #[test]
    fn oversized_inbound_frame_is_rejected_before_allocation() {
        let mut codec = FrameCodec::with_limits(16, 32);
        let mut input = Cursor::new(33_u32.to_le_bytes());

        let result = codec.read_sync::<Frame<HostRequest, PluginResponse>, _>(&mut input);

        assert!(matches!(result, Err(FrameError::FrameTooLarge(33))));
        assert!(codec.decode_capacity() < 33);
    }

    #[test]
    fn oversized_outbound_frame_is_rejected() {
        let mut codec = FrameCodec::with_limits(16, 8);
        let value: Frame<HostRequest, PluginResponse> = Frame::Notify {
            body: HostRequest::CommandInvoke {
                command: crate::contract::CommandHandle::from_raw(
                    std::num::NonZeroU64::new(1).unwrap(),
                ),
                args: Vec::new(),
            },
        };
        let mut output = Vec::new();

        let result = codec.write_sync(&mut output, &value);

        assert!(matches!(result, Err(FrameError::FrameTooLarge(_))));
        assert!(output.is_empty());
    }

    #[test]
    fn huge_frame_round_trip() {
        let msg = "x".repeat(256 * 1024);
        let value: Frame<PluginRequest, HostResponse> = Frame::Notify {
            body: PluginRequest::Log {
                level: LogLevel::Info,
                plugin: "fixture".into(),
                msg,
            },
        };
        let mut codec = FrameCodec::with_floor(1024);
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &value).unwrap();

        let decoded: Frame<PluginRequest, HostResponse> =
            codec.read_sync(&mut Cursor::new(bytes)).unwrap();
        let Frame::Notify {
            body: PluginRequest::Log { msg, .. },
        } = decoded
        else {
            panic!("unexpected frame");
        };
        assert_eq!(msg.len(), 256 * 1024);
    }

    #[test]
    fn buffers_reuse_capacity_after_warmup() {
        let value: Frame<HostRequest, PluginResponse> = Frame::Notify {
            body: HostRequest::Shutdown,
        };
        let mut codec = FrameCodec::with_floor(32);
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &value).unwrap();
        let encode_cap = codec.encode_capacity();
        let _: Frame<HostRequest, PluginResponse> =
            codec.read_sync(&mut Cursor::new(bytes.clone())).unwrap();
        let decode_cap = codec.decode_capacity();

        for _ in 0..8 {
            let start_encode = codec.encode_capacity();
            let start_decode = codec.decode_capacity();
            let mut out = Vec::new();
            codec.write_sync(&mut out, &value).unwrap();
            let _: Frame<HostRequest, PluginResponse> =
                codec.read_sync(&mut Cursor::new(bytes.clone())).unwrap();
            assert_eq!(codec.encode_capacity(), start_encode);
            assert_eq!(codec.decode_capacity(), start_decode);
        }

        assert_eq!(codec.encode_capacity(), encode_cap);
        assert_eq!(codec.decode_capacity(), decode_cap);
    }
}
