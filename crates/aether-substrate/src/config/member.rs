//! What one config type contributes to the composition-derived chassis
//! aggregate (ADR-0156 §4): the [`ConfigMemberRecord`] declaration
//! `#[derive(aether_substrate::Config)]` emits, and the [`ConfigMember`] trait
//! the composition boundary bounds a cap's `Config` by.

use std::any::TypeId;

use confique::meta::Meta;

use super::error::ConfigError;
use super::sources::ConfigSources;

/// One config member's compose-stage declaration (ADR-0156 §4): the TOML
/// config-file section it reads and the confique [`Meta`] carrying its
/// `AETHER_*` keys, defaults, and docs. Both fields are `'static`, so the
/// record is `Copy` and rides the [`Builder`](crate::chassis::builder::Builder)'s
/// accumulator as an ordinary value.
#[derive(Clone, Copy, Debug)]
pub struct ConfigMemberRecord {
    /// The `[section]` name this member reads from the sectioned TOML
    /// chassis config file (e.g. `"http"`, `"scheduler"`). Declared on the
    /// config type via the derive's `#[config(section = "...")]` (defaulting
    /// to `cli_prefix` where they align).
    pub section: &'static str,
    /// The confique layer `Meta` — the walk reads its leaves for the env
    /// keys, defaults, and docs.
    pub meta: &'static Meta,
    /// The config type's [`TypeId`] (ADR-0156 §5). Lets
    /// [`ConfigManifest::provenance`](crate::ConfigManifest::provenance) correlate a member with the
    /// [`ConfigSources`] stack — the programmatic and argv layers are keyed by
    /// config `TypeId` — so the manifest can report which layer supplied each
    /// resolved member.
    pub type_id: TypeId,
    /// The member's derive-emitted argv overlay long flags (ADR-0162), one per
    /// `#[derive(Config)]` field — the exact `--flags` the sibling `*Overlay`
    /// accepts, without leading dashes (`"http-timeout-ms"`). The
    /// `#[derive(Config)]` machinery computes this from the same `cli_long`
    /// resolution it stamps onto the overlay's `#[arg(long = …)]`, so the
    /// reported argv surface can never drift from the accepted flags. The
    /// composition-derived [`ConfigManifest::argv_flags`](crate::ConfigManifest::argv_flags) unions these across
    /// every composed member; the hand `()` member contributes none.
    pub cli_flags: &'static [&'static str],
}

/// ADR-0156 §4 member trait: a config type's contribution to the
/// composition-derived chassis config aggregate. `#[derive(aether_substrate::Config)]`
/// emits the impl (one member carrying the type's section + `META`). The
/// composition boundary ([`Builder::with_actor`](crate::chassis::builder::Builder::with_actor))
/// bounds a cap's `Config` by this trait, so a cap that smuggles construction
/// wiring into `Config` (a live handle, a resolved `MailboxId`) — a type the
/// derive can't apply to — stops compiling at the compose site rather than
/// drifting silently out of the aggregate.
///
/// [`members`](Self::members) is a **required method with no default** — a
/// blanket empty default would make the bound vacuous (any `impl ConfigMember
/// for T {}` one-liner would satisfy it, re-opening the exact wiring-in-`Config`
/// escape hatch the bound exists to close). Rust cannot seal a trait a derive
/// must implement downstream, so the required-method shape is the enforcement:
/// after #3849 the only hand-written impl in the workspace is `()` below
/// (`RpcServerConfig`'s pre-#3849 programmatic bridge retired — its port now
/// resolves through the source stack via the derive), everything else is
/// derive-emitted or moved its wiring to `Params` (`Config = ()`).
pub trait ConfigMember {
    /// The member records this config declares. A derive-`Config` type returns
    /// its one section + `META` record; the sanctioned `()` hand impl returns
    /// empty.
    #[must_use]
    fn members() -> Vec<ConfigMemberRecord>;

    /// ADR-0156 §5: resolve this member's value from the builder's source
    /// stack — programmatic > argv > env > file > default. The composition
    /// boundary calls this once per composed member ahead of `init`, so the
    /// builder (not the chassis) owns resolution: section identity comes from
    /// the derive declaration (never a chassis-side string), and the layer
    /// precedence is byte-identical to the pre-inversion per-cap
    /// `resolve_with_file` path. A derive-`Config` type delegates to
    /// [`ConfigSources::resolve_layered`] with its own section; the sanctioned
    /// `()` hand impl resolves trivially.
    ///
    /// A **required method with no default** for the same reason as
    /// [`members`](Self::members): a blanket default would re-open the
    /// wiring-in-`Config` escape hatch the required-method shape closes.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known env key, argv overlay value, or
    /// config-file section holds an unparseable value (ADR-0090 §4 — the
    /// hard-error half stays a hard boot error).
    fn resolve(sources: &mut ConfigSources) -> Result<Self, ConfigError>
    where
        Self: Sized;
}

/// The configless-cap case: a cap whose `Config = ()` declares no aggregate
/// member. The one sanctioned hand impl (after #3849 retired the
/// `RpcServerConfig` bridge, it is the only one); every other non-derive
/// `Config` moved its wiring onto the `Params` channel rather than stamp an
/// empty member impl here.
impl ConfigMember for () {
    fn members() -> Vec<ConfigMemberRecord> {
        Vec::new()
    }

    fn resolve(_sources: &mut ConfigSources) -> Result<Self, ConfigError> {
        Ok(())
    }
}
