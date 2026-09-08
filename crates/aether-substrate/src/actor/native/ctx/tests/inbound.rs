//! The request-context half of the inbound frame: a reply's correlation
//! recovers the typed context the request stored, exactly once.

use std::sync::Arc;

use aether_data::{MailId, MailboxId, RequestId};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::mailbox::NativeActorMailbox;
use crate::mail::{Source, SourceAddr};

use super::support::{CastOnly, NativeRequestContext, StubActor};

#[test]
fn native_ctx_take_context_consumes_stored_reply_context() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF10)));
    binding.store_request_context(RequestId(77), &NativeRequestContext { value: 9 });

    let reply_source = Source::with_correlation(SourceAddr::None, 77);
    let mut ctx = NativeCtx::new(&binding, reply_source, MailId::NONE, MailId::NONE);

    assert_eq!(ctx.take_context::<NativeRequestContext>(), Some(NativeRequestContext { value: 9 }));
    assert_eq!(ctx.take_context::<NativeRequestContext>(), None);
}

#[test]
fn native_actor_mailbox_send_with_context_stores_by_minted_correlation() {
    use crate::testing::bare_substrate;

    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF11)));
    let mailbox = NativeActorMailbox::<'_, StubActor>::__new_in_flight(0x00FE_ED01, &binding, None, None);
    let context = NativeRequestContext { value: 13 };

    let mail_id = mailbox.with_context(&context).send(&CastOnly { code: 1 });

    assert_eq!(binding.take_request_context::<NativeRequestContext>(RequestId(mail_id.correlation_id)), Some(context));
}
