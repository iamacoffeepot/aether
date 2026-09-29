//! The pumped slot the desktop backend runs in, which only a desktop boot
//! mints.

use std::sync::Arc;

use aether_actor::Single;
use aether_substrate::actor::native::{NativeCtx, PumpedSlot};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{DriverCtx, MailboxWakeSlot};

use super::{DesktopWindowBoot, DesktopWindows};
use crate::runtime::WindowBackend;
use crate::{WindowCapability, WindowParams};

/// [`WindowCapability`] booted pumped with the desktop backend, on the
/// application thread whose winit callbacks realize its host actions.
///
/// [`Self::boot`] is the only way to mint one, so a host turn through it
/// always reaches the desktop backend: the application never handles a
/// synthetic manager, and the desktop backend never runs pooled.
pub struct DesktopWindowSlot {
    slot: PumpedSlot<WindowCapability>,
}

impl DesktopWindowSlot {
    /// Boot [`WindowCapability`] pumped with the desktop backend, from the
    /// `aether.window` mailbox the driver reserved at the Claim stage.
    /// Returns the slot plus the claim's wake, which the driver points at its
    /// event loop.
    ///
    /// # Errors
    ///
    /// The driver reserved no `aether.window` mailbox, another type holds the
    /// namespace, or the actor's `init` failed (see
    /// [`DriverCtx::boot_pumped_actor`]).
    pub fn boot(ctx: &mut DriverCtx<'_>, app_name: String) -> Result<(Self, Arc<MailboxWakeSlot>), BootError> {
        let params = WindowParams::Desktop(DesktopWindowBoot { app_name });
        let (slot, wake) = ctx.boot_pumped_actor::<WindowCapability>((), params)?;
        Ok((Self { slot }, wake))
    }

    /// Dispatch every mail already queued on the manager.
    pub(super) fn drain_available(&mut self) {
        self.slot.drain_available();
    }

    /// Run the manager's closed path.
    pub(super) fn shutdown(&mut self) {
        self.slot.shutdown();
    }

    /// Run `turn` against the desktop backend on a host turn, or answer
    /// `None` once the manager is no longer live. A slot only a desktop boot
    /// mints holds no other backend, so the projection never misses.
    pub(super) fn host_turn<R>(
        &mut self,
        turn: impl FnOnce(&mut DesktopWindows, &mut NativeCtx<'_, WindowCapability, Single>) -> R,
    ) -> Option<R> {
        self.slot
            .host_turn(|state, ctx| match &mut state.backend {
                WindowBackend::Desktop(windows) => Some(turn(windows, ctx)),
                #[cfg(feature = "synthetic")]
                WindowBackend::Synthetic(_) => None,
            })
            .flatten()
    }
}
