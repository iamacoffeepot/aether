use crate::wire::PeerKind;
use aether_data::MailboxId;
use aether_substrate::config::{ConfigError, ConfigMember, ConfigMemberRecord, ConfigSources};

/// Init config for `RpcServerCapability`.
///
/// `bind_addr` carries the cap's configured/disabled state (ADR-0155 §3):
/// `Some(addr)` — e.g. `Some("127.0.0.1:8910".into())`, `"0.0.0.0:0"` to
/// let the OS pick — binds the listener and starts the accept thread;
/// `None` composes the cap disabled — it claims its `aether.rpc.server`
/// mailbox but binds no socket and spawns no listener. The bind address
/// is itself the configured signal (there is no separate `enabled` flag,
/// mirroring `HttpServerConfig::enabled` at the address level), so an
/// unconfigured chassis (`AETHER_RPC_PORT` absent) passes `None` and the
/// mailbox still exists to answer mail rather than warn-drop it.
///
/// ADR-0156 §3: `bind_addr` is the operator-resolvable `Config`; the
/// construction wiring (`peer_kind` identity and the forward `route_target`)
/// is composer-supplied and rides [`RpcServerParams`] instead.
pub struct RpcServerConfig {
    pub bind_addr: Option<String>,
}

// ADR-0156 §4: `RpcServerConfig` is the one composed `Config` that is neither
// a `#[derive(aether_substrate::Config)]` type nor `()` — `bind_addr` is
// resolved from `AETHER_RPC_PORT` outside the derive path, and migrating that
// resolution into a derive-`Config` member (with its own `META` + `[rpc]`
// section) is the `AETHER_RPC_PORT` slice owned by #3849. This is the pre-#3849
// bridge: the one sanctioned non-`()` hand impl (explicitly empty), and it dies
// when the port knob migrates onto a derive-`Config` member. Until then the
// port key stays claimed via the `CHASSIS_KNOBS` hand record.
impl ConfigMember for RpcServerConfig {
    fn members() -> Vec<ConfigMemberRecord> {
        Vec::new()
    }

    /// ADR-0156 §5: the programmatic-only bridge. `bind_addr` is resolved from
    /// `AETHER_RPC_PORT` outside the derive path (in the chassis) and staged as
    /// a programmatic override via `Builder::with_config`, so this member has
    /// no argv/env/file layer of its own — it takes the override, defaulting to
    /// the disabled (`None`) address if a composer forgot to stage one. Dies
    /// with the hand impl when the port knob migrates onto a derive-`Config`
    /// member (#3849).
    fn resolve(sources: &mut ConfigSources) -> Result<Self, ConfigError> {
        Ok(sources.take_or(|| RpcServerConfig { bind_addr: None }))
    }
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
