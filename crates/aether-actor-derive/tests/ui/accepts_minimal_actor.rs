//! Baseline: a minimal well-formed `#[actor] impl FfiActor` — one
//! `#[handler]` plus a `const NAMESPACE` — expands cleanly. Guards the
//! reject fixtures against false positives: the new diagnostics must
//! fire on the malformed shapes, not on every actor.

use aether_actor::actor;

#[repr(C)]
#[derive(
    Copy,
    Clone,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "test.ping")]
struct Ping {
    seq: u32,
}

struct Minimal;

#[actor]
impl aether_actor::FfiActor for Minimal {
    const NAMESPACE: &'static str = "minimal";

    fn init(_ctx: &mut aether_actor::FfiInitCtx<'_>) -> Result<Self, aether_actor::BootError>
    {
        Ok(Minimal)
    }

    #[handler]
    fn on_ping(&mut self, _ctx: &mut aether_actor::FfiCtx<'_>, _ping: Ping) {}
}

fn main() {}
