//! Registry reads off a ctx: a proven reference answers its actor's
//! canonical path, before and after that actor departs.

use std::sync::Arc;

use aether_data::ActorPath;

use crate::actor::native::NativeCtx;
use crate::mail::registry::{InboxHandler, OwnedDispatch};
use crate::mail::{Source, SourceAddr};
use crate::testing::{bare_substrate, drop_ref, registered_binding, registered_ref};

fn discharging() -> Arc<dyn InboxHandler> {
    Arc::new(|dispatch: OwnedDispatch| dispatch.discharge())
}

/// The path is read from a route of any lifecycle, so a peer that has since
/// departed is still named: the dropped-mailbox case is the one a log line
/// most needs a name for, and a lookup that read only `Live` routes would
/// blank it.
#[test]
fn actor_path_names_a_peer_before_and_after_it_departs() {
    let (registry, mailer) = bare_substrate();
    let (binding, _caller) = registered_binding(&registry, &mailer, "test.native.path_host", discharging());
    let peer_name = "test.native.path_peer";
    let peer = registered_ref(&registry, peer_name, discharging());
    let expected = ActorPath::new(peer_name).expect("the peer name is a canonical path");
    let ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 0), None, None);

    assert_eq!(ctx.actor_path(peer), expected);

    drop_ref(&registry, peer);

    assert_eq!(ctx.actor_path(peer), expected);
}
