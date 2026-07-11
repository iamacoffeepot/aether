//! Init configs for the TCP listener and session actors (ADR-0090).
//! Both are child-actor init bundles carrying raw `std::net` handles
//! (`TcpListener` / `TcpStream`), consumed only by the runtime halves,
//! so the module rides the `feature = "runtime"` gate.

use std::net::{TcpListener, TcpStream};

/// Init config for [`TcpListenerActor`](super::TcpListenerActor).
/// `TcpCapability::on_bind` binds the socket on the dispatcher thread
/// (so addr-parse / port-in-use failures surface synchronously) and
/// hands the bound listener through `spawn_child`. The `listener`
/// field is `Option` so init can move it out into the accept thread.
pub struct TcpListenerConfig {
    pub listener: Option<TcpListener>,
    pub addr: String,
    pub port: u16,
    pub consumer: Option<String>,
}

/// Init config for [`TcpSessionActor`](super::TcpSessionActor). A listener's
/// `on_connection_ready` builds it for an accepted stream; the cap's
/// `on_connect_ready` builds the same config for a dialed stream. `stream` is
/// `Option` so init can `.take()` and split it; `peer`, `session_name`, and the
/// optional late-bound `consumer` are shared by both session lineages.
pub struct TcpSessionConfig {
    pub stream: Option<TcpStream>,
    pub peer: String,
    pub session_name: String,
    pub consumer: Option<String>,
}
