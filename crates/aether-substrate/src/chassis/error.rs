//! Chassis-side boot failure types.
//!
//! `BootError` is what every `Capability::boot` / `NativeActor::init` /
//! `Chassis::build` returns on failure (per ADR-0063 a boot error
//! aborts the chassis before user code runs — no partial boots).

use std::error::Error as StdError;
use std::fmt;
#[cfg(feature = "wasm")]
use std::io;

use crate::mail::registry::NameConflict;

/// Failure modes capability boot can raise. Per ADR-0063, any boot
/// error aborts the chassis before user code runs — no partial boots.
#[derive(Debug)]
pub enum BootError {
    /// The mailbox name is already bound, either to another
    /// capability that claimed it earlier or to a legacy
    /// `Registry::register_inbox` / `Registry::register_inline`
    /// call from `SubstrateBoot::build`.
    /// Phase 2-5 expect this during the side-by-side period and
    /// remove the legacy registration in the same diff.
    MailboxAlreadyClaimed { name: String },
    /// A second capability tried to register a fallback router after
    /// one was already installed. The slot is single-claim by design.
    FallbackRouterAlreadyClaimed,
    /// Anything else a capability's boot wants to surface.
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MailboxAlreadyClaimed { name } => {
                write!(f, "mailbox {name:?} already claimed")
            }
            Self::FallbackRouterAlreadyClaimed => {
                f.write_str("fallback router slot already claimed")
            }
            Self::Other(e) => write!(f, "capability boot failed: {e}"),
        }
    }
}

impl StdError for BootError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Other(e) => Some(&**e),
            _ => None,
        }
    }
}

impl From<NameConflict> for BootError {
    fn from(e: NameConflict) -> Self {
        Self::MailboxAlreadyClaimed { name: e.name }
    }
}

/// Forward `wasmtime::Error` into [`BootError::Other`]. Used by every
/// boot-path that bubbles a wasmtime fault: `Engine::new` / `Module::new`
/// in `SubstrateBoot::build`, etc. Chassis trait impls can `?` such
/// errors directly without per-call `.map_err` glue.
#[cfg(feature = "wasm")]
impl From<wasmtime::Error> for BootError {
    fn from(e: wasmtime::Error) -> Self {
        Self::Other(Box::new(io::Error::other(format!("{e}"))))
    }
}
