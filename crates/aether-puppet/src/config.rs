//! The puppet's own init-config vocabulary (ADR-0090).
//!
//! Two fields, both optional, and an omitted one means `None`.
//!
//! The subject exists because of what a stranger sees. A puppet
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
//!
//! The hatch style is the same move for the drawing's look: the field is
//! the [`Hatch`] kind, so a shipped product can carry the style an
//! operator would otherwise mail.

use crate::{Hatch, Load};

/// Init-config for [`Puppet`](crate::Puppet).
///
/// # Agent
/// Encode one of these to the puppet's `Config` shape and pass it as the
/// `config` bytes of the `aether.component.load` that instantiates it (or
/// `load_component`'s `config` / `config_path`). Omitting config bytes
/// boots [`PuppetConfig::default()`] — no subject and the authored style,
/// which is the pre-config behaviour: a live puppet waiting to be pointed
/// at one. Either field may be left out of the JSON; an omitted field is
/// `None`. A `hatch` that [`Hatch::is_solvable`] refuses fails the load.
#[aether_data::kind(name = "aether.puppet.config", default, partial_eq)]
pub struct PuppetConfig {
    /// The subject to load at `wire`, exactly as an `aether.puppet.load`
    /// would name it. `None` waits for the mail instead.
    ///
    /// The load stays asynchronous — `wire` issues the reads and returns —
    /// so the first frames draw the empty camera and the subject appears
    /// when its bytes settle, the same way a mailed load behaves.
    pub subject: Option<Load>,
    /// The hatch style to draw with, exactly as an `aether.puppet.hatch`
    /// would carry it. `None` keeps the authored style.
    ///
    /// Applied in `init`, before any subject is loaded, so nothing is
    /// re-extracted for it. A style the mail would refuse — a non-finite
    /// field, a spacing that is not positive, a light of zero length —
    /// refuses the load instead: at instantiation there is no earlier
    /// style to keep, and a bad checked-in config should fail where it is
    /// read.
    pub hatch: Option<Hatch>,
}
