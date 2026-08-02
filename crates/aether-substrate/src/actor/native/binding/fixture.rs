//! Shared fixtures for the per-concept test modules beside it.

use std::sync::Arc;
use std::sync::mpsc;

use super::NativeBinding;
use crate::actor::native::envelope::Envelope;
use crate::mail::MailboxId;
use crate::mail::registry::{InboxHandler, OwnedDispatch};

#[cfg(feature = "wasm")]
use crate::actor::wasm::component::ComponentCtx;
#[cfg(feature = "wasm")]
use crate::mail::mailer::Mailer;
#[cfg(feature = "wasm")]
use crate::mail::{HubOutbound, Registry};

/// Build a registry handler that forwards every [`OwnedDispatch`]
/// it receives onto `tx` as an owned [`Envelope`]. Used by tests
/// that need a registered recipient but only care about
/// observing — or just not warn-dropping — the mail.
pub(super) fn forward_to_envelope_sender(tx: mpsc::Sender<Envelope>) -> Arc<dyn InboxHandler> {
    // iamacoffeepot/aether#848: the helper takes
    // [`OwnedDispatch`] directly so payload + kind_name move
    // into the forwarded [`Envelope`] without `to_vec()` /
    // `to_owned()` clones.
    Arc::new(move |dispatch: OwnedDispatch| {
        // ADR-0094: this test sink is the terminal consumer (there is
        // no real downstream dispatcher to discharge), so discharge
        // the obligation here before forwarding the value for the
        // test to observe — otherwise the observing `drop(env)` would
        // trip the debug guard.
        dispatch.discharge();
        // `Envelope` is now a type alias for `OwnedDispatch`, so
        // the inbox-handler value moves straight onto the actor
        // mpsc with no field-by-field translation.
        let _ = tx.send(dispatch);
    })
}

#[cfg(feature = "wasm")]
pub(super) fn component_ctx_with_binding(
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    sender: MailboxId,
) -> (ComponentCtx, Arc<NativeBinding>) {
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), sender));
    let mut ctx = ComponentCtx::new(sender, registry, mailer, HubOutbound::disconnected());
    ctx.install_binding(Arc::clone(&binding));
    (ctx, binding)
}
