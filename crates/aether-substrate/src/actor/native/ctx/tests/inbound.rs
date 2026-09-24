//! The request-context half of the inbound frame: a reply's correlation
//! recovers the typed context the request stored, exactly once.

use std::sync::Arc;

use aether_data::{MailboxId, RequestId};

use crate::actor::native::NativeCtx;
use crate::actor::native::binding::NativeBinding;
use crate::mail::{Source, SourceAddr};

use super::support::NativeRequestContext;

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
