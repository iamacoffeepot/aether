//! Issue 1958 / ADR-0230: the component-origin half of the `ctx.sender()`
//! fixture pair.
//!
//! `SourceForwarder` declares `SourceObserver` as a dependency, so the host
//! refuses its load unless the observer already holds a `Live` route beneath
//! the same component host. That declaration *is* the proof: the forwarder
//! mints an `ActorRef<SourceObserver>` from it with no host call and no
//! address in the mail, which is why [`SendSourceQuery`] is fieldless.
//!
//! The forward makes this actor the component origin, so the observer's
//! `ctx.sender()` reads the forwarder's own `MailboxId` — the property
//! `aether-component`'s source-attribution scenario asserts.
//!
//! A second actor rather than a self-dependency: the observer is loaded twice
//! in that scenario (a "reader" and, before this split, a "sender"), and a
//! type cannot declare itself. `Embedded` resolution folds the dependency
//! under the shared component-host parent, so the forwarder reaches the
//! observer at the observer's default load name.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and this forwarder is stateless — the same allow `source_observer` and `matrix_sweep` already carry

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{SendSourceQuery, SourceQuery};

use super::source_observer::SourceObserver;

pub struct SourceForwarder;

#[actor(depends(SourceObserver))]
impl WasmActor for SourceForwarder {
    const NAMESPACE: &'static str = "test.source_forwarder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(SourceForwarder)
    }

    /// Forward `SourceQuery` to the declared observer. The handler spells its
    /// actor type because `actor_ref` is bounded `A: DependsOn<R>` and so does
    /// not exist on the erased ctx.
    #[handler::single]
    fn on_send_source_query(&mut self, ctx: &mut WasmCtx<'_, SourceForwarder>, _msg: SendSourceQuery) {
        let observer = ctx.actor_ref::<SourceObserver>();
        ctx.to(&observer).send(&SourceQuery);
    }
}
