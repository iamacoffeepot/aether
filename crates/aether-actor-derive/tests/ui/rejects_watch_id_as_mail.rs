//! A `WatchId` has actor reach (ADR-0242, ADR-0079 §8): it names a row in
//! its holder's own watch table, and the same number in another actor's hands
//! names one of that actor's own watches. A kind holding one may be its
//! holder's saved state, and is never mail, so a handler for it fails to
//! compile.

use aether_actor::{WatchId, actor};

#[derive(Clone, Debug, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.reach.watch_release")]
struct WatchRelease {
    watch: WatchId,
}

struct Ledger;

#[actor]
impl aether_actor::WasmActor for Ledger {
    const NAMESPACE: &'static str = "ledger";

    fn init(_ctx: &mut aether_actor::WasmInitCtx<'_>) -> Result<Self, aether_actor::ActorInitError> {
        Ok(Ledger)
    }

    #[handler::tell]
    fn on_release(&mut self, _ctx: &mut aether_actor::WasmCtx<'_>, _release: WatchRelease) {}
}

fn main() {}
