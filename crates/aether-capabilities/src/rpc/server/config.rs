use aether_rpc::rpc::PeerKind;

/// Init config for `RpcServerCapability`.
///
/// `bind_addr` is the address to bind on (e.g. `"127.0.0.1:8910"`,
/// `"0.0.0.0:0"` to let the OS pick). `peer_kind` identifies this
/// server to connecting peers via the `HelloAck` reply; chassis
/// builders supply a `PeerKind::Substrate { engine_name, .. }` for
/// substrate / hub endpoints.
pub struct RpcServerConfig {
    pub bind_addr: String,
    pub peer_kind: PeerKind,
}
