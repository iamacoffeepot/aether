//! The request-context half of the inbound frame: a reply's correlation
//! recovers the typed context, or the typed held reply, the request stored,
//! exactly once.

use std::sync::Arc;

use aether_data::{MailboxId, RequestId};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, NativeRequestContext};

#[test]
fn native_ctx_take_context_consumes_stored_reply_context() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF10)));
    binding.store_request_context(RequestId(77), &NativeRequestContext { value: 9 });

    let reply_source = Source::with_correlation(SourceAddr::None, 77);
    let mut ctx = NativeCtx::new(&binding, reply_source, None, None);

    assert_eq!(ctx.take_context::<NativeRequestContext>(), Some(NativeRequestContext { value: 9 }));
    assert_eq!(ctx.take_context::<NativeRequestContext>(), None);
}

/// A held reply parked under a request comes back only to a reply to that
/// request, and only as the kind it was held as: a wrong-kind probe leaves
/// it parked, so a handler that serves two reply shapes can try each in turn.
#[test]
fn native_ctx_take_held_matches_kind_and_correlation() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF11)));

    let (_pending, held) = NativeCtx::new(&binding, Source::NONE, None, None).hold::<CastOnly>();
    let (kind, reply) = held.into_keyed();
    binding.store_held(RequestId(77), kind, reply);

    let mut ordinary = NativeCtx::new(&binding, Source::NONE, None, None);
    assert!(ordinary.take_held::<CastOnly>().is_none(), "mail that answers no request takes nothing");

    let mut unrelated = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 78), None, None);
    assert!(unrelated.take_held::<CastOnly>().is_none(), "a reply to another request takes nothing");

    let mut reply_ctx = NativeCtx::new(&binding, Source::with_correlation(SourceAddr::None, 77), None, None);
    assert!(reply_ctx.take_held::<NativeRequestContext>().is_none(), "a wrong-kind probe takes nothing");

    reply_ctx
        .take_held::<CastOnly>()
        .expect("the wrong-kind probe left the held reply parked")
        .abandon_for_actor_close();
    assert!(reply_ctx.take_held::<CastOnly>().is_none(), "a held reply is taken once");
}
