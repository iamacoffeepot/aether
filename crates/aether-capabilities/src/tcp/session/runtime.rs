//! The `aether.tcp.session` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpSessionActor`](super::TcpSessionActor) identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config through the
//! single `use runtime::*` glob in the parent.

pub use std::io::{Read, Write};
pub use std::net::{Shutdown, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::{self, JoinHandle};

pub use aether_data::Kind;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::{KindId, Mail, Mailer};

pub use crate::tcp::config::TcpSessionConfig;

/// Default per-read buffer size. 64 KiB matches the typical
/// kernel TCP buffer; any larger and we just block waiting for
/// the kernel to fill it. Smaller adds syscall overhead per
/// chunk.
pub const READ_BUFFER_BYTES: usize = 64 * 1024;

/// `aether.tcp.session` runtime state (issue 607 Phase 6b, ADR-0079). One end
/// of a split `TcpStream`: the read sidecar owns the read half; the dispatcher
/// owns `write_half` (used by `on_session_write`). Read-side errors / EOF flow
/// back to the dispatcher via the `bytes_rx` channel as `Err(reason)`; the
/// dispatcher discards them today (issue 775 retired the
/// `SessionData` / `SessionClosed` broadcast path). The addressing identity is
/// the distinct ZST [`TcpSessionActor`](super::TcpSessionActor).
pub struct TcpSessionState {
    pub(super) peer: String,
    pub(super) session_name: String,
    pub(super) write_half: TcpStream,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) read_thread: Option<JoinHandle<()>>,
    pub(super) bytes_rx: mpsc::Receiver<Result<Vec<u8>, String>>,
}
