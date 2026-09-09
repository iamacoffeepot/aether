//! The puppet's own init-config vocabulary (ADR-0090).
//!
//! One field, and it exists because of what a stranger sees. A puppet
//! loaded with no config is a correct actor and a blank window: it owns its
//! camera, publishes a projection every frame, and draws nothing until
//! somebody mails it `aether.puppet.load`. That somebody is an operator or
//! an MCP session — which is exactly what a shipped product does not have.
//!
//! So the subject a load would name can be named at instantiation instead,
//! and the puppet issues that load itself in `wire`. It is the same load:
//! the field is the [`Load`] kind, not a second spelling of it, so anything
//! the mail path can point at the config can point at too, and the two
//! cannot drift.

use crate::Load;

/// Init-config for [`Puppet`](crate::Puppet).
///
/// # Agent
/// Encode one of these to the puppet's `Config` shape and pass it as the
/// `config` bytes of the `aether.component.load` that instantiates it (or
/// `load_component`'s `config` / `config_path`). Omitting config bytes
/// boots [`PuppetConfig::default()`] — no subject, which is the pre-config
/// behaviour: a live puppet waiting to be pointed at one.
#[aether_data::kind(name = "aether.puppet.config", default, partial_eq)]
pub struct PuppetConfig {
    /// The subject to load at `wire`, exactly as an `aether.puppet.load`
    /// would name it. `None` waits for the mail instead.
    ///
    /// The load stays asynchronous — `wire` issues the reads and returns —
    /// so the first frames draw the empty camera and the subject appears
    /// when its bytes settle, the same way a mailed load behaves.
    pub subject: Option<Load>,
}
