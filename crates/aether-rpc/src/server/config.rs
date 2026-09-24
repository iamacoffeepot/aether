use crate::wire::PeerKind;

/// Init config for `RpcServerCapability` (ADR-0156 §3/§4, ADR-0155 §3).
///
/// `port` is the operator-resolvable knob, resolved through the builder's
/// source stack like any `#[derive(aether_substrate::Config)]` member: argv
/// (`--rpc-port`) > env (`AETHER_RPC_PORT`) > the `[rpc]` config-file section >
/// default. `Some(port)` binds `aether.rpc.server` on `127.0.0.1:{port}` (port
/// `0` lets the OS pick) and starts the accept thread, when
/// [`RpcServerParams::bind`] says; `None` (unset) composes
/// the cap disabled — it claims its mailbox but binds no socket and spawns no
/// listener, so mail arriving there is answered rather than warn-dropped at an
/// unknown mailbox. The port's presence is itself the enable signal (no
/// separate flag), mirroring `HttpServerConfig` at the address level.
///
/// Desktop / headless leave it unset (unbound); the hub composes it explicitly
/// with its `DEFAULT_RPC_PORT` fallback via `Builder::with_actor_configured`.
/// The peer-identity wiring rides [`RpcServerParams`], never here
/// (ADR-0156 §3).
///
/// Before #3849 `bind_addr` was resolved from `AETHER_RPC_PORT` outside the
/// derive path and staged programmatically through a hand `ConfigMember` bridge
/// impl; that bridge is gone — the port now resolves through the source stack
/// like every other member, and this derive emits the `ConfigMember` impl.
#[derive(Clone, Debug, Default, aether_substrate::Config)]
#[config(env_prefix = "AETHER_RPC", cli_prefix = "rpc")]
pub struct RpcServerConfig {
    /// Localhost port the RPC server binds; unset disables it, 0 picks any free port.
    ///
    /// Binds `aether.rpc.server` on this loopback port. Unset composes the
    /// cap disabled (claimed, unbound); `0` binds an OS-assigned ephemeral
    /// port.
    #[config(env = "AETHER_RPC_PORT")]
    pub port: Option<u16>,
    /// Path the server writes its bound port to once it is listening; unset writes nothing.
    ///
    /// The file holds the port in decimal and appears atomically when the
    /// server becomes reachable: in `init` under [`RpcBind::Boot`], when
    /// its gate opens under [`RpcBind::Held`]. A hub that forks a substrate
    /// on port `0` names this file and dials only the port it reports.
    #[config(env = "AETHER_RPC_PORT_FILE")]
    pub port_file: Option<String>,
}

/// When a server composed with a resolved port binds its listener (issue
/// #6399). The port stays the operator knob that decides whether any socket
/// binds at all (ADR-0155 §3); this is composer wiring that decides only when.
/// A server composed without a port binds nothing in either mode.
#[derive(Clone, Copy, Debug)]
pub enum RpcBind {
    /// Bind the listener and start accepting inside `init`, during the
    /// chassis build, and publish the `RpcServerHandle` there.
    Boot,
    /// Bind nothing inside `init` and publish an `RpcBindGate` instead. The
    /// composer opens the gate once everything a caller may address is live,
    /// so a dial before that is refused and reachable means ready.
    Held,
}

/// Composer-supplied construction params for `RpcServerCapability`
/// (ADR-0156 §3). `peer_kind` identifies this server to connecting peers via
/// the `HelloAck` reply; chassis builders supply a `PeerKind::Substrate {
/// engine_name, .. }` for substrate / hub endpoints. Engine-addressed
/// forwarding is not configured here: each engine's proxy registers its
/// own route with the running server (`RegisterEngineRoute`).
pub struct RpcServerParams {
    pub peer_kind: PeerKind,
    /// When a resolved port binds: during the build, or when the composer
    /// opens the published `RpcBindGate`.
    pub bind: RpcBind,
}
