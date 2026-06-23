//! Init configs for the TCP listener and session actors (ADR-0090).
//! Both carry `std::net` types and are native-only.

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
}

/// Init config for [`TcpSessionActor`](super::TcpSessionActor). The
/// listener's `on_connection_ready` builds this per accepted stream
/// and hands it through `spawn_child`. `stream` is `Option` so init
/// can `.take()` and split it; `peer` and `session_name` are retained
/// for log attribution.
pub struct TcpSessionConfig {
    pub stream: Option<TcpStream>,
    pub peer: String,
    pub session_name: String,
}
