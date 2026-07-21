use crate::wire::PeerKind;
use aether_data::MailboxId;

/// Init config for `RpcServerCapability` (ADR-0156 §3/§4, ADR-0155 §3).
///
/// `port` is the operator-resolvable knob, resolved through the builder's
/// source stack like any `#[derive(aether_substrate::Config)]` member: argv
/// (`--rpc-port`) > env (`AETHER_RPC_PORT`) > the `[rpc]` config-file section >
/// default. `Some(port)` binds `aether.rpc.server` on `127.0.0.1:{port}` (port
/// `0` lets the OS pick) and starts the accept thread; `None` (unset) composes
/// the cap disabled — it claims its mailbox but binds no socket and spawns no
/// listener, so mail arriving there is answered rather than warn-dropped at an
/// unknown mailbox. The port's presence is itself the enable signal (no
/// separate flag), mirroring `HttpServerConfig` at the address level.
///
/// Desktop / headless leave it unset (unbound); the hub composes it explicitly
/// with its `DEFAULT_RPC_PORT` fallback via `Builder::with_actor_configured`.
/// The peer-identity / forward-route wiring rides [`RpcServerParams`], never
/// here (ADR-0156 §3).
///
/// Before #3849 `bind_addr` was resolved from `AETHER_RPC_PORT` outside the
/// derive path and staged programmatically through a hand `ConfigMember` bridge
/// impl; that bridge is gone — the port now resolves through the source stack
/// like every other member, and this derive emits the `ConfigMember` impl.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RPC", cli_prefix = "rpc")]
pub struct RpcServerConfig {
    /// `AETHER_RPC_PORT=<port>` / `--rpc-port <port>` localhost port
    /// `aether.rpc.server` binds. Unset composes the cap disabled (claimed,
    /// unbound); `0` binds an OS-assigned ephemeral port.
    #[config(env = "AETHER_RPC_PORT")]
    pub port: Option<u16>,
}

/// Composer-supplied construction params for `RpcServerCapability`
/// (ADR-0156 §3). `peer_kind` identifies this server to connecting peers via
/// the `HelloAck` reply; chassis builders supply a `PeerKind::Substrate {
/// engine_name, .. }` for substrate / hub endpoints. `route_target` is a
/// resolved mailbox id — by definition `Params`, never `Config`.
pub struct RpcServerParams {
    pub peer_kind: PeerKind,
    /// Mailbox that envelope-requested forwards (`to.engine.is_some()`) route to.
    /// `None` on chassis that don't forward — the branch drops, as today.
    pub route_target: Option<MailboxId>,
}
