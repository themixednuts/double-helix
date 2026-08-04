//! Bounded length-prefixed transport shared by Helix process boundaries.
//!
//! The codec deliberately knows nothing about request IDs, protocol versions,
//! or application errors. Those belong to each wire contract. A frame is a
//! little-endian `u32` byte length followed by compact msgpack.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const BUILD_TARGET: &str = env!("BUILD_TARGET");
pub const VERSION_AND_GIT_HASH: &str = env!("VERSION_AND_GIT_HASH");

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

/// Allocation-reusing codec for one ordered byte stream.
///
/// A codec must not be shared by concurrent readers or writers. Use one codec
/// per stream direction so encoding and decoding never contend on a mutex.
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
    use std::io::Cursor;

    #[test]
    fn round_trip_and_reuse_buffers() {
        let value = vec!["alpha", "beta"];
        let mut codec = FrameCodec::with_floor(32);
        let mut bytes = Vec::new();
        codec.write_sync(&mut bytes, &value).unwrap();
        let encoded_capacity = codec.encode_capacity();

        let decoded: Vec<String> = codec.read_sync(&mut Cursor::new(bytes)).unwrap();
        let decoded_capacity = codec.decode_capacity();

        assert_eq!(decoded, value);
        assert_eq!(codec.encode_capacity(), encoded_capacity);
        assert_eq!(codec.decode_capacity(), decoded_capacity);
    }

    #[test]
    fn rejects_oversized_input_before_allocating() {
        let mut codec = FrameCodec::with_limits(16, 32);
        let result = codec.read_sync::<Vec<u8>, _>(&mut Cursor::new(33_u32.to_le_bytes()));

        assert!(matches!(result, Err(FrameError::FrameTooLarge(33))));
        assert!(codec.decode_capacity() < 33);
    }
}
