//! Stdio RPC transport for out-of-process plugin runtimes.
//!
//! Frames are a little-endian `u32` byte length followed by a compact msgpack
//! body. [`FrameCodec`] owns reusable encode/decode buffers so steady-state
//! traffic does not allocate for every message.

mod host_runner;
mod protocol;

pub use host_runner::run_plugin_host;
pub use protocol::*;

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const HEADER_LEN: usize = 4;
const DEFAULT_FLOOR: usize = 8 * 1024;
const DEFAULT_MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("msgpack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type FrameResult<T> = std::result::Result<T, FrameError>;

/// Reusable length-prefixed msgpack frame codec.
#[derive(Debug)]
pub struct FrameCodec {
    encode: Vec<u8>,
    decode: Vec<u8>,
    floor: usize,
    max_frame_len: usize,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::with_floor(DEFAULT_FLOOR)
    }
}

impl FrameCodec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_floor(floor: usize) -> Self {
        Self::with_limits(floor, DEFAULT_MAX_FRAME_LEN)
    }

    pub fn with_limits(floor: usize, max_frame_len: usize) -> Self {
        let floor = floor.min(max_frame_len);
        Self {
            encode: Vec::with_capacity(floor.max(HEADER_LEN)),
            decode: Vec::with_capacity(floor),
            floor,
            max_frame_len,
        }
    }

    pub fn encode_capacity(&self) -> usize {
        self.encode.capacity()
    }

    pub fn decode_capacity(&self) -> usize {
        self.decode.capacity()
    }

    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    pub fn clear(&mut self) {
        self.encode.clear();
        self.decode.clear();
        self.encode.shrink_to(self.floor.max(HEADER_LEN));
        self.decode.shrink_to(self.floor);
    }

    fn encode_frame<T: Serialize>(&mut self, value: &T) -> FrameResult<&[u8]> {
        self.encode.clear();
        self.encode.resize(HEADER_LEN, 0);
        value.serialize(&mut rmp_serde::Serializer::new(&mut self.encode))?;
        let len = self.encode.len() - HEADER_LEN;
        if len > self.max_frame_len || len > u32::MAX as usize {
            return Err(FrameError::FrameTooLarge(len));
        }
        self.encode[..HEADER_LEN].copy_from_slice(&(len as u32).to_le_bytes());
        Ok(&self.encode)
    }

    pub async fn write<T, W>(&mut self, writer: &mut W, value: &T) -> FrameResult<()>
    where
        T: Serialize,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        let frame = self.encode_frame(value)?;
        writer.write_all(frame).await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn read<'a, T, R>(&'a mut self, reader: &mut R) -> FrameResult<T>
    where
        T: Deserialize<'a>,
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;

        let mut header = [0; HEADER_LEN];
        reader.read_exact(&mut header).await?;
        let len = u32::from_le_bytes(header) as usize;
        if len > self.max_frame_len {
            return Err(FrameError::FrameTooLarge(len));
        }
        self.decode.resize(len, 0);
        reader.read_exact(&mut self.decode).await?;
        Ok(rmp_serde::from_slice(&self.decode)?)
    }

    pub fn write_sync<T, W>(&mut self, writer: &mut W, value: &T) -> FrameResult<()>
    where
        T: Serialize,
        W: Write,
    {
        let frame = self.encode_frame(value)?;
        writer.write_all(frame)?;
        writer.flush()?;
        Ok(())
    }

    pub fn read_sync<'a, T, R>(&'a mut self, reader: &mut R) -> FrameResult<T>
    where
        T: Deserialize<'a>,
        R: Read,
    {
        let mut header = [0; HEADER_LEN];
        reader.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header) as usize;
        if len > self.max_frame_len {
            return Err(FrameError::FrameTooLarge(len));
        }
        self.decode.resize(len, 0);
        reader.read_exact(&mut self.decode)?;
        Ok(rmp_serde::from_slice(&self.decode)?)
    }
}

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
