//! ADR-0045 typed-handle SDK demo. Publishes a `Note` value into the
//! substrate's handle store on the first tick, then broadcasts a
//! `HeldNote` whose `held: Ref<Note>` field is the handle's wire
//! reference. The substrate's dispatch path resolves the handle to
//! its inline form before forwarding through `hub.claude.broadcast`,
//! so attached Claude sessions see a fully-inline `HeldNote`.
//!
//! Run via MCP:
//!
//! 1. `spawn_substrate` a desktop / headless chassis with this
//!    component preloaded.
//! 2. `receive_mail` — one `demo.handle.held_note` frame surfaces
//!    on the first tick, with `held` arriving as the `Inline`
//!    variant even though the wire reference was a `Handle`.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers, resolve_sink};
use aether_kinds::Tick;
use aether_mail::Ref;

#[derive(
    aether_mail::Kind, aether_mail::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "demo.handle.note")]
pub struct Note {
    pub body: String,
    pub seq: u32,
}

#[derive(
    aether_mail::Kind, aether_mail::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "demo.handle.held_note")]
pub struct HeldNote {
    pub held: Ref<Note>,
    pub seq: u32,
}

const BROADCAST: Sink<HeldNote> = resolve_sink::<HeldNote>("hub.claude.broadcast");

pub struct HandleDemo {
    fired: bool,
}

#[handlers]
impl Component for HandleDemo {
    fn init(_ctx: &mut InitCtx<'_>) -> Self {
        HandleDemo { fired: false }
    }

    /// First-tick demo: publish a `Note`, broadcast a `HeldNote`
    /// whose `held` is a `Ref::Handle`, pin the entry so it
    /// outlives the local guard.
    ///
    /// # Agent
    /// Not typically sent manually; the substrate's tick loop fires
    /// this. Watch `receive_mail` for a `demo.handle.held_note`
    /// frame. The `held` field arrives as `{"Inline": { ... }}`
    /// because the substrate resolved the handle on dispatch.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        if self.fired {
            return;
        }
        self.fired = true;

        let inner = Note {
            body: String::from("from a handle"),
            seq: 7,
        };
        let Some(handle) = ctx.publish(&inner) else {
            return;
        };
        let outer = HeldNote {
            held: handle.as_ref(),
            seq: 11,
        };
        // Broadcast is fire-and-forget. By the time the hub forwards
        // to a Claude session the local guard would have been
        // released and the entry could be evicted under pressure —
        // pin it so the cached bytes survive.
        handle.pin();
        BROADCAST.send_postcard(&outer);
        // `handle` drops here; the pin keeps the cached entry alive
        // past release.
    }
}

aether_component::export!(HandleDemo);
