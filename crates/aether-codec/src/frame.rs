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

use std::env;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::OnceLock;

use serde::{Serialize, de::DeserializeOwned};
use std::error;

/// Maximum accepted frame body size, default. Bounded so a malformed
/// length prefix cannot drive a reader into an OOM. 64 MiB is large
/// enough that routine debug wasm cross-builds (typically 15-25 MiB
/// for the medium-size components in this repo) ride the framing
/// without tripping the OOM guard, but still small enough to defang a
/// 4 GiB malformed prefix.
///
/// Embedders shipping bigger payloads override this via the
/// `AETHER_MAX_FRAME_SIZE` env var; see [`max_frame_size`].
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Hard upper bound on the env-var override. Even with
/// `AETHER_MAX_FRAME_SIZE` set, the accessor clamps at 1 GiB so a
/// runaway override can't itself defeat the OOM guard.
pub const MAX_FRAME_SIZE_CEILING: usize = 1024 * 1024 * 1024;

/// Effective maximum frame body size for this process. Resolves once
/// per process: reads the `AETHER_MAX_FRAME_SIZE` env var on the first
/// call (parsed as bytes, clamped at [`MAX_FRAME_SIZE_CEILING`]) and
/// caches the result; subsequent calls return the cache. A missing,
/// empty, or unparseable env var falls back to [`MAX_FRAME_SIZE`].
///
/// The encode-side check ([`encode_frame`]) and the read-side check
/// ([`read_frame`]) both go through this accessor, so changing
/// `AETHER_MAX_FRAME_SIZE` at process start lifts the cap symmetrically
/// for both sides.
#[must_use]
// Process-level wire-framing tuning knob (AETHER_MAX_FRAME_SIZE), read once at the
// codec layer below the actor/config system — not cap config.
#[allow(clippy::disallowed_methods)]
pub fn max_frame_size() -> usize {
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| resolve_max_frame_size(env::var("AETHER_MAX_FRAME_SIZE").ok()))
}

/// Pure-function half of [`max_frame_size`]: maps an optional env-var
/// string to the resolved cap. Exposed for unit tests since the
/// `OnceLock` cache in [`max_frame_size`] is process-global and can't
/// be re-set across tests.
fn resolve_max_frame_size(raw: Option<String>) -> usize {
    raw.map_or(MAX_FRAME_SIZE, |raw| match raw.trim().parse::<usize>() {
        Ok(n) if n > 0 => n.min(MAX_FRAME_SIZE_CEILING),
        _ => MAX_FRAME_SIZE,
    })
}

/// Errors from the framing helpers. Wraps I/O and postcard decode
/// errors; adds its own variants for an oversize length prefix on
/// inbound frames ([`FrameError::FrameTooLarge`]) and a pre-write
/// oversize check on outbound bodies ([`FrameError::EncodeTooLarge`]).
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Postcard(postcard::Error),
    /// Inbound: length prefix exceeded [`max_frame_size`].
    FrameTooLarge {
        size: usize,
        max: usize,
    },
    /// Outbound: the postcard-encoded body exceeded [`max_frame_size`].
    /// Surfaced from [`encode_frame`] / [`write_frame`] so the sender
    /// learns the rejection client-side instead of writing a frame
    /// the peer will reject (or drop the connection over).
    EncodeTooLarge {
        size: usize,
        max: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "frame io: {e}"),
            Self::Postcard(e) => write!(f, "frame decode: {e}"),
            Self::FrameTooLarge { size, max } => {
                write!(f, "frame too large: {size} > {max}")
            }
            Self::EncodeTooLarge { size, max } => {
                write!(f, "encoded frame too large: {size} > {max}")
            }
        }
    }
}

impl error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<postcard::Error> for FrameError {
    fn from(e: postcard::Error) -> Self {
        Self::Postcard(e)
    }
}

/// Encode a message into its framed wire representation (4-byte LE
/// length prefix + postcard body). Returns
/// [`FrameError::EncodeTooLarge`] if the encoded body exceeds
/// [`max_frame_size`], so the sender learns the rejection client-side
/// instead of writing a frame the peer will reject. Postcard encoding
/// of `alloc::Vec` is itself infallible for the types this is used
/// with, so the postcard step is a `.expect` rather than an `Err`
/// path per ADR-0063.
///
/// # Panics
/// Panics if postcard encoding of `msg` fails — fail-fast per ADR-0063:
/// `postcard::to_allocvec` into a growable `Vec` cannot fail for the
/// `Serialize` types this is used with, so a failure indicates the
/// caller passed a type whose serializer is observably broken.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_allocvec(msg).expect("postcard encode to Vec is infallible");
    let max = max_frame_size();
    if body.len() > max {
        return Err(FrameError::EncodeTooLarge {
            size: body.len(),
            max,
        });
    }
    let mut out = Vec::with_capacity(4 + body.len());
    // 4-byte LE length prefix is the wire format; bodies above 4 GiB
    // would overflow but the cap above keeps us well clear of the u32
    // ceiling.
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Synchronous read of one framed message. Blocks until a complete
/// frame is consumed from `r`. Async callers should inline the
/// length+body reads on their own async stream rather than calling
/// this on a blocking wrapper.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let max = max_frame_size();
    if len > max {
        return Err(FrameError::FrameTooLarge { size: len, max });
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(postcard::from_bytes(&buf)?)
}

/// Synchronous write of one framed message. Returns
/// [`FrameError::EncodeTooLarge`] if the encoded body exceeds
/// [`max_frame_size`] (the encode-side cap check).
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let bytes = encode_frame(msg)?;
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
        write_frame(&mut buf, &Msg::Tick).expect("test setup: write unit frame");
        let back: Msg = read_frame(&mut Cursor::new(buf)).expect("test setup: read unit frame");
        assert_eq!(back, Msg::Tick);
    }

    #[test]
    fn roundtrip_struct_variant() {
        let msg = Msg::Note {
            id: 7,
            text: "hi".into(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).expect("test setup: write struct frame");
        let back: Msg = read_frame(&mut Cursor::new(buf)).expect("test setup: read struct frame");
        assert_eq!(back, msg);
    }

    #[test]
    fn unit_variant_is_five_bytes() {
        // 4 byte prefix + 1 byte postcard tag.
        assert_eq!(
            encode_frame(&Msg::Tick)
                .expect("test setup: encode unit variant")
                .len(),
            5,
        );
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Tick).expect("test setup: write first tick");
        write_frame(
            &mut buf,
            &Msg::Note {
                id: 1,
                text: "a".into(),
            },
        )
        .expect("test setup: write note frame");
        write_frame(&mut buf, &Msg::Tick).expect("test setup: write second tick");

        let mut r = Cursor::new(buf);
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 1 of 3");
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 2 of 3");
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 3 of 3");
    }

    #[test]
    fn frame_too_large_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        let err =
            read_frame::<_, Msg>(&mut Cursor::new(buf)).expect_err("oversized frame must reject");
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[test]
    fn truncated_body_returns_io_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf))
            .expect_err("truncated body must surface io error");
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[test]
    fn malformed_body_returns_postcard_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0xff);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf))
            .expect_err("malformed body must surface postcard error");
        assert!(matches!(err, FrameError::Postcard(_)));
    }

    /// An oversize encode body trips `FrameError::EncodeTooLarge`
    /// before any bytes hit the writer.
    #[test]
    fn encode_too_large_rejected_pre_write() {
        // Build a `Note` whose text alone exceeds the resolved cap.
        // The encode-side check sees the postcard-encoded body length
        // and bails before allocating the framed `Vec`.
        let oversize_text = "x".repeat(max_frame_size() + 16);
        let msg = Msg::Note {
            id: 1,
            text: oversize_text,
        };
        let err = encode_frame(&msg).expect_err("oversize body must reject on encode");
        let max = max_frame_size();
        match err {
            FrameError::EncodeTooLarge { size, max: cap } => {
                assert!(size > max, "size {size} must exceed cap {max}");
                assert_eq!(cap, max);
            }
            other => panic!("expected EncodeTooLarge, got {other:?}"),
        }
    }

    /// `write_frame` propagates `EncodeTooLarge` without touching the
    /// underlying writer.
    #[test]
    fn write_frame_propagates_encode_too_large() {
        let oversize_text = "x".repeat(max_frame_size() + 16);
        let msg = Msg::Note {
            id: 1,
            text: oversize_text,
        };
        let mut sink: Vec<u8> = Vec::new();
        let err = write_frame(&mut sink, &msg).expect_err("oversize write must reject");
        assert!(matches!(err, FrameError::EncodeTooLarge { .. }));
        assert!(
            sink.is_empty(),
            "oversize encode must not write partial bytes; got {} bytes",
            sink.len(),
        );
    }

    /// The `AETHER_MAX_FRAME_SIZE` env-var override goes through
    /// `resolve_max_frame_size`. The `OnceLock` cache in
    /// `max_frame_size` is process-global and can't be re-set across
    /// tests, so this exercises the parsing layer directly.
    #[test]
    fn env_override_parses_and_clamps() {
        // Unset / empty / garbage → default.
        assert_eq!(resolve_max_frame_size(None), MAX_FRAME_SIZE);
        assert_eq!(resolve_max_frame_size(Some(String::new())), MAX_FRAME_SIZE);
        assert_eq!(
            resolve_max_frame_size(Some("not-a-number".into())),
            MAX_FRAME_SIZE,
        );
        assert_eq!(resolve_max_frame_size(Some("0".into())), MAX_FRAME_SIZE);
        // Valid override.
        let override_val: usize = 32 * 1024 * 1024;
        assert_eq!(
            resolve_max_frame_size(Some(override_val.to_string())),
            override_val,
        );
        // Whitespace tolerated.
        assert_eq!(resolve_max_frame_size(Some("  4096  ".into())), 4096);
        // Above ceiling → clamps.
        let above_ceiling: usize = MAX_FRAME_SIZE_CEILING * 4;
        assert_eq!(
            resolve_max_frame_size(Some(above_ceiling.to_string())),
            MAX_FRAME_SIZE_CEILING,
        );
    }
}
