//! Chassis-side boot failure types.
//!
//! `BootError` is what every `Capability::boot` / `NativeActor::init` /
//! `Chassis::build` returns on failure (per ADR-0063 a boot error
//! aborts the chassis before user code runs — no partial boots).
//! `WedgedFrameBound` is the diagnostic the per-frame drain barrier
//! returns when a frame-bound capability's inbox didn't drain within
//! the budget.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use crate::mail::MailboxId;
use crate::mail::registry::NameConflict;

/// Failure modes capability boot can raise. Per ADR-0063, any boot
/// error aborts the chassis before user code runs — no partial boots.
#[derive(Debug)]
pub enum BootError {
    /// The mailbox name is already bound, either to another
    /// capability that claimed it earlier or to a legacy
    /// `Registry::register_closure` call from `SubstrateBoot::build`.
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
            BootError::MailboxAlreadyClaimed { name } => {
                write!(f, "mailbox {name:?} already claimed")
            }
            BootError::FallbackRouterAlreadyClaimed => {
                f.write_str("fallback router slot already claimed")
            }
            BootError::Other(e) => write!(f, "capability boot failed: {e}"),
        }
    }
}

impl StdError for BootError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            BootError::Other(e) => Some(&**e),
            _ => None,
        }
    }
}

impl From<NameConflict> for BootError {
    fn from(e: NameConflict) -> Self {
        BootError::MailboxAlreadyClaimed { name: e.name }
    }
}

/// Forward `anyhow::Error` (which `wasmtime::Error` is a re-export
/// of) into [`BootError::Other`]. Used by every boot-path that
/// bubbles a catch-all error: wasmtime calls in `SubstrateBoot::build`,
/// the chassis-bundle's `connect_hub_client` (anyhow over TCP), etc.
/// Chassis trait impls can `?` either kind of error directly without
/// per-call `.map_err` glue.
impl From<wasmtime::Error> for BootError {
    fn from(e: wasmtime::Error) -> Self {
        BootError::Other(Box::new(std::io::Error::other(format!("{e}"))))
    }
}

/// Diagnostic returned from `BootedChassis::drain_frame_bound` when
/// a frame-bound capability's inbox didn't drain within the budget.
/// The chassis frame loop routes this through `lifecycle::fatal_abort`
/// the same way component-side wedges do — see
/// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`].
#[derive(Debug, Clone, Copy)]
pub struct WedgedFrameBound {
    pub mailbox: MailboxId,
    pub pending: u64,
    pub waited: Duration,
}

impl fmt::Display for WedgedFrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "frame-bound dispatcher wedged: mailbox={} pending={} waited={:?}",
            self.mailbox, self.pending, self.waited,
        )
    }
}
