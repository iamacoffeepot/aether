//! The `aether.tcp.listener` runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpListenerActor`](super::TcpListenerActor) identity never names these
//! types nor pulls `aether_substrate`. The substrate / `std::net`-typed
//! imports are gated once by this module rather than line-by-line; the
//! `#[actor] impl` reaches the state, ctx types, and config / session types
//! through the single `use runtime::*` glob in the parent.

pub use std::net::{SocketAddr, TcpStream};
pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::sync::mpsc;
pub use std::thread::{self, JoinHandle};
pub use std::time::Duration;

pub use aether_data::Kind;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::{KindId, Mail, Mailer};

pub use crate::tcp::config::{TcpListenerConfig, TcpSessionConfig};
pub use crate::tcp::session::TcpSessionActor;

/// `aether.tcp.listener` runtime state (issue 607 Phase 6b, ADR-0079). The
/// accept thread can't call `ctx.spawn_child` (no dispatcher ctx), so it
/// pushes accepted streams over `connection_rx` and fires a
/// [`ConnectionReady`](super::ConnectionReady) wake mail. The dispatcher's
/// `on_connection_ready` handler drains the mpsc and spawns one
/// `TcpSessionActor` per pending stream. The addressing identity is the
/// distinct ZST [`TcpListenerActor`](super::TcpListenerActor).
pub struct TcpListenerState {
    pub(super) local_port: u16,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) accept_thread: Option<JoinHandle<()>>,
    pub(super) connection_rx: mpsc::Receiver<(TcpStream, SocketAddr)>,
    pub(super) next_subname: u64,
}
