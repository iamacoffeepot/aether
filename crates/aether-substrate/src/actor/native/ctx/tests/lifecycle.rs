//! Monitoring off a ctx whose binding carries no spawner.

use std::sync::Arc;
use std::sync::mpsc;

use aether_data::{Kind, Source, SourceAddr};

use crate::actor::native::ctx::NativeCtx;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::testing::{bare_substrate, registered_binding, registered_ref};

use super::support::StubActor;

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}

/// A binding built for a test, with no spawner, is a full watcher: its
/// monitor lands in the lifecycle table its registry owns, which is the
/// table a chassis over the same routes closes against, and a target that
/// had already closed is posted to the binding's own mailbox. A binding
/// with a table of its own, or none, would hold an entry no close drains
/// and hear of no departure.
#[test]
fn a_binding_with_no_spawner_monitors_in_its_registry_s_table() {
    let (registry, mailer) = bare_substrate();
    let (posted, notices) = mpsc::channel();
    let recording: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
        let _ = posted.send((dispatch.kind, dispatch.sender));
        dispatch.discharge();
    });
    let (binding, watcher) = registered_binding(&registry, &mailer, "test.native_ctx.watcher", recording);
    let live = registered_ref(&registry, "test.native_ctx.watched", discharging());
    let closed = registered_ref(&registry, "test.native_ctx.closed", discharging());
    let _ = registry.actor_registry().close_actor(closed.id());
    let ctx = NativeCtx::<StubActor>::new_for_actor(&binding, Source::NONE, None, None);

    let watching = ctx.monitor(live);
    let _late = ctx.monitor(closed);

    assert_eq!(
        notices.try_iter().collect::<Vec<_>>(),
        [(aether_kinds::MonitorNotice::ID, Source::to(SourceAddr::Component(closed.id())))],
        "the closed target's notice, and only it, is on the watcher's mailbox",
    );
    assert_eq!(
        registry.actor_registry().close_actor(live.id()),
        [watcher.id()],
        "the registry's own table holds the live target's entry",
    );
    drop(watching);
}
