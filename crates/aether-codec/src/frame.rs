//! Length-prefixed stream framing for serde-derived message types.
//!
//! Each frame on the wire is a 4-byte little-endian body length
//! followed by the `aether_data::wire`-encoded message body (ADR-0118).
//! Two enum types per protocol typically enforce direction at the type
//! level; the helpers here are generic over `<T: Serialize>` /
//! `<T: DeserializeOwned>` so any wire-derived enum can ride
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
//! The body uses `aether_data::wire` (ADR-0118). When a second body
//! format arrives, the right shape is to subdivide this module into
//! `frame::wire` / `frame::protobuf` siblings rather than
//! parameterising the existing helpers — most callers know which format
//! their protocol speaks at compile time.

use std::error;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::OnceLock;

use aether_data::wire;
use serde::{Serialize, de::DeserializeOwned};

/// Maximum accepted frame body size, default. Bounded so a malformed
/// length prefix cannot drive a reader into an OOM. 64 MiB is large
/// enough that routine debug wasm cross-builds (typically 15-25 MiB
/// for the medium-size components in this repo) ride the framing
/// without tripping the OOM guard, but still small enough to defang a
/// 4 GiB malformed prefix.
///
/// Embedders shipping bigger payloads raise the cap through the config
/// member that pushes it in ([`install_max_frame_size`]) — the codec never
/// reads the environment itself (ADR-0156 §6).
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

/// Hard upper bound on the installed cap. Even a large
/// [`install_max_frame_size`] push is clamped at 1 GiB so a runaway override
/// can't itself defeat the OOM guard.
pub const MAX_FRAME_SIZE_CEILING: usize = 1024 * 1024 * 1024;

/// The process-wide installed frame cap. `None` until the boot path pushes a
/// resolved value; [`max_frame_size`] falls back to [`MAX_FRAME_SIZE`] while
/// it stays empty. `aether-codec` sits below the actor/config system and
/// cannot resolve config itself (ADR-0156 §6), so the value arrives by push,
/// not pull.
static INSTALLED_MAX_FRAME_SIZE: OnceLock<usize> = OnceLock::new();

/// The outcome of an [`install_into`] attempt. Drives the redundant-install
/// warning and lets the set-once/clamp/fallback wiring be unit-tested against
/// a fresh `OnceLock` without touching the process-global cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutcome {
    /// This call installed `clamped` — it was the first push into the cell.
    Installed(usize),
    /// A value was already present; this push was ignored. Carries the
    /// still-authoritative installed value.
    AlreadyInstalled(usize),
}

/// Push a resolved maximum frame body size into `cell`, clamped to
/// [`MAX_FRAME_SIZE_CEILING`]. Set-once: the first push wins and every later
/// push is reported as [`InstallOutcome::AlreadyInstalled`] without changing
/// the cell.
fn install_into(cell: &OnceLock<usize>, bytes: usize) -> InstallOutcome {
    let clamped = bytes.min(MAX_FRAME_SIZE_CEILING);
    match cell.set(clamped) {
        Ok(()) => InstallOutcome::Installed(clamped),
        Err(_) => InstallOutcome::AlreadyInstalled(*cell.get().unwrap_or(&MAX_FRAME_SIZE)),
    }
}

/// Read `cell`, falling back to the compiled [`MAX_FRAME_SIZE`] default when
/// nothing has been installed.
fn resolve_from(cell: &OnceLock<usize>) -> usize {
    *cell.get().unwrap_or(&MAX_FRAME_SIZE)
}

/// Install the process-wide maximum frame body size (ADR-0156 §6). Each
/// chassis and `aether-mcp` resolves the frame-size config member through its
/// own source stack and pushes the value here once at boot, before any framing
/// runs, so the codec never reads the environment. The value is
/// clamped to [`MAX_FRAME_SIZE_CEILING`], so an over-large push cannot defeat
/// the OOM guard regardless of the pusher.
///
/// Set-once: the first install wins for the process lifetime. A second install
/// is ignored — the frame cap is a wire invariant read on both the encode and
/// decode sides, so changing it once framing has begun would let one side
/// encode a frame the other rejects. A redundant push is a boot-ordering bug,
/// not a value to honor, and is too benign to abort a process over; a push
/// that disagrees with the installed value is warned so the misordering is
/// diagnosable, while a same-value re-push stays a silent no-op.
pub fn install_max_frame_size(bytes: usize) {
    if let InstallOutcome::AlreadyInstalled(installed) = install_into(&INSTALLED_MAX_FRAME_SIZE, bytes) {
        let clamped = bytes.min(MAX_FRAME_SIZE_CEILING);
        if installed != clamped {
            tracing::warn!(
                target: "aether_codec::frame",
                installed,
                ignored = clamped,
                "max frame size already installed; ignoring redundant install with a different value",
            );
        }
    }
}

/// Effective maximum frame body size for this process — the value pushed by
/// [`install_max_frame_size`], or the compiled [`MAX_FRAME_SIZE`] default
/// before any install.
///
/// The encode-side check ([`encode_frame`]) and the read-side check
/// ([`read_frame`]) both go through this accessor, so a single install lifts
/// the cap symmetrically for both sides.
#[must_use]
pub fn max_frame_size() -> usize {
    resolve_from(&INSTALLED_MAX_FRAME_SIZE)
}

/// Errors from the framing helpers. Wraps I/O and wire decode
/// errors; adds its own variants for an oversize length prefix on
/// inbound frames ([`FrameError::FrameTooLarge`]) and a pre-write
/// oversize check on outbound bodies ([`FrameError::EncodeTooLarge`]).
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Wire(wire::Error),
    /// Inbound: length prefix exceeded [`max_frame_size`].
    FrameTooLarge {
        size: usize,
        max: usize,
    },
    /// Outbound: the wire-encoded body exceeded [`max_frame_size`].
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
            Self::Wire(e) => write!(f, "frame decode: {e}"),
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

impl From<wire::Error> for FrameError {
    fn from(e: wire::Error) -> Self {
        Self::Wire(e)
    }
}

/// Encode a message into its framed wire representation (4-byte LE
/// length prefix + `aether_data::wire` body). Returns
/// [`FrameError::EncodeTooLarge`] if the encoded body exceeds
/// [`max_frame_size`], so the sender learns the rejection client-side
/// instead of writing a frame the peer will reject. Wire encoding
/// of a `Vec` is itself infallible for the types this is used
/// with, so the wire step is a `.expect` rather than an `Err`
/// path per ADR-0063.
///
/// # Panics
/// Panics if wire encoding of `msg` fails — fail-fast per ADR-0063:
/// `wire::to_vec` into a growable `Vec` cannot fail for the
/// `Serialize` types this is used with, so a failure indicates the
/// caller passed a type whose serializer is observably broken.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = wire::to_vec(msg).expect("wire encode to Vec is infallible");
    let max = max_frame_size();
    if body.len() > max {
        return Err(FrameError::EncodeTooLarge { size: body.len(), max });
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
    Ok(wire::from_bytes(&buf)?)
}

/// Pop one complete length-prefixed frame body from an incremental
/// receive buffer. A partial prefix or body returns `Ok(None)` without
/// changing `buf`, so callers can append the next stream chunk and try
/// again. An oversize prefix is rejected before waiting for or
/// allocating its declared body.
pub fn pop_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, FrameError> {
    let Some(&prefix) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(prefix) as usize;
    let max = max_frame_size();
    if len > max {
        return Err(FrameError::FrameTooLarge { size: len, max });
    }

    let frame_len = 4 + len;
    if buf.len() < frame_len {
        return Ok(None);
    }

    Ok(Some(buf.drain(..frame_len).skip(4).collect()))
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
        let msg = Msg::Note { id: 7, text: "hi".into() };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).expect("test setup: write struct frame");
        let back: Msg = read_frame(&mut Cursor::new(buf)).expect("test setup: read struct frame");
        assert_eq!(back, msg);
    }

    #[test]
    fn unit_variant_is_eight_bytes() {
        // 4 byte prefix + 4 byte wire body (the variant index u32; the
        // image is unversioned, ADR-0118 §Envelope).
        assert_eq!(encode_frame(&Msg::Tick).expect("test setup: encode unit variant").len(), 8,);
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Tick).expect("test setup: write first tick");
        write_frame(&mut buf, &Msg::Note { id: 1, text: "a".into() }).expect("test setup: write note frame");
        write_frame(&mut buf, &Msg::Tick).expect("test setup: write second tick");

        let mut r = Cursor::new(buf);
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 1 of 3");
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 2 of 3");
        let _: Msg = read_frame(&mut r).expect("test setup: read frame 3 of 3");
    }

    fn raw_frame(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(4 + body.len());
        let body_len = u32::try_from(body.len()).expect("test frame body fits the wire prefix");
        frame.extend_from_slice(&body_len.to_le_bytes());
        frame.extend_from_slice(body);
        frame
    }

    #[test]
    fn pop_frame_returns_exact_single_body() {
        let body = b"one frame";
        let mut buf = raw_frame(body);

        assert_eq!(pop_frame(&mut buf).expect("complete frame parses"), Some(body.to_vec()));
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_frame_retains_partial_buffer() {
        let mut buf = raw_frame(b"partial body");
        buf.truncate(buf.len() - 2);
        let retained = buf.clone();

        assert_eq!(pop_frame(&mut buf).expect("partial frame is not an error"), None);
        assert_eq!(buf, retained);
    }

    #[test]
    fn pop_frame_drains_two_back_to_back_frames() {
        let mut buf = raw_frame(b"first");
        buf.extend_from_slice(&raw_frame(b"second"));
        let mut bodies = Vec::new();

        while let Some(body) = pop_frame(&mut buf).expect("buffer contains valid frames") {
            bodies.push(body);
        }

        assert_eq!(bodies, vec![b"first".to_vec(), b"second".to_vec()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_frame_reassembles_body_split_across_appends() {
        let frame = raw_frame(b"split body");
        let split = frame.len() - 3;
        let mut buf = frame[..split].to_vec();

        assert_eq!(pop_frame(&mut buf).expect("partial frame is not an error"), None);
        buf.extend_from_slice(&frame[split..]);
        assert_eq!(pop_frame(&mut buf).expect("completed frame parses"), Some(b"split body".to_vec()));
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_frame_rejects_oversize_prefix() {
        let oversize = max_frame_size() + 1;
        let oversize_prefix = u32::try_from(oversize).expect("test oversize value fits the wire prefix");
        let mut buf = oversize_prefix.to_le_bytes().to_vec();

        let err = pop_frame(&mut buf).expect_err("oversize prefix must reject");
        assert!(matches!(err, FrameError::FrameTooLarge { size, max } if size == oversize && max == max_frame_size()));
    }

    #[test]
    fn frame_too_large_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).expect_err("oversized frame must reject");
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[test]
    fn truncated_body_returns_io_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 10]);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).expect_err("truncated body must surface io error");
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[test]
    fn malformed_body_returns_wire_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0xff);
        let err = read_frame::<_, Msg>(&mut Cursor::new(buf)).expect_err("malformed body must surface wire error");
        assert!(matches!(err, FrameError::Wire(_)));
    }

    /// An oversize encode body trips `FrameError::EncodeTooLarge`
    /// before any bytes hit the writer.
    #[test]
    fn encode_too_large_rejected_pre_write() {
        // Build a `Note` whose text alone exceeds the resolved cap.
        // The encode-side check sees the wire-encoded body length
        // and bails before allocating the framed `Vec`.
        let oversize_text = "x".repeat(max_frame_size() + 16);
        let msg = Msg::Note { id: 1, text: oversize_text };
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
        let msg = Msg::Note { id: 1, text: oversize_text };
        let mut sink: Vec<u8> = Vec::new();
        let err = write_frame(&mut sink, &msg).expect_err("oversize write must reject");
        assert!(matches!(err, FrameError::EncodeTooLarge { .. }));
        assert!(sink.is_empty(), "oversize encode must not write partial bytes; got {} bytes", sink.len());
    }

    /// The install/read wiring (ADR-0156 §6), exercised against a fresh
    /// `OnceLock` so the assertions never touch — nor are perturbed by — the
    /// process-global `INSTALLED_MAX_FRAME_SIZE` the rejection tests read.
    ///
    /// Tripwire: pins the set-once + clamp + default-fallback contract. Drifts
    /// if a push stops winning first, a second push starts overwriting, the
    /// ceiling clamp is dropped, or the uninstalled read stops falling back to
    /// `MAX_FRAME_SIZE`.
    #[test]
    fn install_is_set_once_clamped_and_defaults() {
        // Before any install the read falls back to the compiled default.
        let cell = OnceLock::new();
        assert_eq!(resolve_from(&cell), MAX_FRAME_SIZE);

        // First push wins and is reported as installed.
        let override_val: usize = 32 * 1024 * 1024;
        assert_eq!(install_into(&cell, override_val), InstallOutcome::Installed(override_val));
        assert_eq!(resolve_from(&cell), override_val);

        // A later, differing push is ignored; the first value stays authoritative.
        assert_eq!(install_into(&cell, 4096), InstallOutcome::AlreadyInstalled(override_val));
        assert_eq!(resolve_from(&cell), override_val);
    }

    #[test]
    fn install_clamps_to_ceiling() {
        // An over-large push is clamped so it cannot defeat the OOM guard.
        let cell = OnceLock::new();
        let above_ceiling: usize = MAX_FRAME_SIZE_CEILING * 4;
        assert_eq!(install_into(&cell, above_ceiling), InstallOutcome::Installed(MAX_FRAME_SIZE_CEILING));
        assert_eq!(resolve_from(&cell), MAX_FRAME_SIZE_CEILING);
    }
}
