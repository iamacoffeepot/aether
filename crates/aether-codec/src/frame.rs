//! Length-prefixed stream framing for serde-derived message types.
//!
//! Each frame on the wire is a 4-byte little-endian body length
//! followed by the postcard-encoded message. Two enum types per
//! protocol typically enforce direction at the type level; the
//! helpers here are generic over `<T: Serialize>` /
//! `<T: DeserializeOwned>` so any postcard-derived enum can ride
//! the same framing.
//!
//! The hub channel (`aether_hub::wire`) is the first consumer.
//! ADR-0072 folded `aether-hub-protocol` into `aether-codec` +
//! `aether-hub`; this module landed in `aether-codec` because
//! length-prefixed streaming is generic codec-shaped machinery, not
//! hub-specific. A future sibling protocol (peer-to-peer, unix-socket,
//! in-process bridge) reuses the same helpers without taking a
//! `aether-hub` dep.
//!
//! Today the body is hardcoded postcard. When a second body format
//! arrives (msgpack, protobuf), the right shape is to subdivide this
//! module into `frame::postcard` / `frame::protobuf` siblings rather
//! than parameterising the existing helpers — most callers know which
//! format their protocol speaks at compile time.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

/// Maximum accepted frame body size. Bounded so a malformed length
/// prefix cannot drive a reader into an OOM. 16 MiB is comfortably
/// larger than any expected mail payload on the hub wire (vertex
/// streams travel through the render sink, not the hub).
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Errors from the framing helpers. Wraps I/O and postcard decode
/// errors; adds its own variant for an oversize length prefix.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Postcard(postcard::Error),
    FrameTooLarge { size: usize, max: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "frame io: {e}"),
            FrameError::Postcard(e) => write!(f, "frame decode: {e}"),
            FrameError::FrameTooLarge { size, max } => {
                write!(f, "frame too large: {size} > {max}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl From<postcard::Error> for FrameError {
    fn from(e: postcard::Error) -> Self {
        FrameError::Postcard(e)
    }
}

/// Encode a message into its framed wire representation (4-byte LE
/// length prefix + postcard body). Infallible — postcard encoding
/// of `alloc::Vec` is infallible for the types this is used with.
pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_allocvec(msg).expect("postcard encode to Vec is infallible");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Synchronous read of one framed message. Blocks until a complete
/// frame is consumed from `r`. Async callers should inline the
/// length+body reads on their own async stream rather than calling
/// this on a blocking wrapper.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(postcard::from_bytes(&buf)?)
}

/// Synchronous write of one framed message.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let bytes = encode_frame(msg);
    w.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    enum Msg {
        Tick,
        Note { id: u32, text: String },
    }

    #[test]
    fn roundtrip_unit_variant() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Tick).unwrap();
        let back: Msg = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(back, Msg::Tick);
    }

    #[test]
    fn roundtrip_struct_variant() {
        let msg = Msg::Note {
            id: 7,
            text: "hi".into(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: Msg = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn unit_variant_is_five_bytes() {
        // 4 byte prefix + 1 byte postcard tag.
        assert_eq!(encode_frame(&Msg::Tick).len(), 5);
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Tick).unwrap();
        write_frame(
            &mut buf,
            &Msg::Note {
                id: 1,
                text: "a".into(),
            },
        )
        .unwrap();
        write_frame(&mut buf, &Msg::Tick).unwrap();

        let mut r = Cursor::new(buf);
        let _: Msg = read_frame(&mut r).unwrap();
        let _: Msg = read_frame(&mut r).unwrap();
        let _: Msg = read_frame(&mut r).unwrap();
    }

    #[test]
    fn frame_too_large_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[test]
    fn truncated_body_returns_io_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[test]
    fn malformed_body_returns_postcard_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0xff);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::Postcard(_)));
    }
}
