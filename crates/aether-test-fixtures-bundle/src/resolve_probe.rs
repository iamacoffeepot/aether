//! ADR-0230 resolution probe fixture (issue #6269).
//!
//! `ResolveProbe` receives a peer name and resolves it through
//! `ctx.resolve`. The probe emits the existing `TickObserved` marker to
//! the substrate-harness observer only when the peer is live, so a
//! harness scenario can load one peer, probe a loaded and an unloaded
//! name, and observe exactly one resolution.

use aether_actor::{ActorInitError, MailSender, WasmActor, WasmCtx, WasmInitCtx, actor, address_named};
use aether_data::LoadName;
use aether_test_fixtures_kinds::{ResolvePeer, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, TickObserved};

use crate::ParentPeerTarget;

pub struct ResolveProbe {
    attempts: u32,
}

#[actor]
impl WasmActor for ResolveProbe {
    const NAMESPACE: &'static str = "test.resolve_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { attempts: 0 })
    }

    #[handler::single]
    fn on_resolve_peer(&mut self, ctx: &mut WasmCtx<'_>, mail: ResolvePeer) {
        self.attempts += 1;
        let ResolvePeer { name } = mail;
        let Ok(name) = LoadName::new(&name) else {
            return;
        };
        if ctx.resolve(&address_named::<ParentPeerTarget>(name)).is_some() {
            ctx.send_to_named::<TickObserved>(SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME, &TickObserved { count: 1 });
        }
    }
}
