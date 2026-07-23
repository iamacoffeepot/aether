//! Fail-fast runtime companion for chassis without a clipboard peripheral.
//! Nested under `runtime` so the one `mod runtime;` gate covers it; the
//! identity ZST lives in the crate-root `headless` module, always-on.

use crate::headless::HeadlessClipboardCapability;
use crate::{GetClipboardText, GetClipboardTextResult, SetClipboardText, SetClipboardTextResult};

use aether_actor::runtime;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

const UNAVAILABLE_ERROR: &str = "unsupported on this chassis — no clipboard peripheral";

/// Stateless runtime for the fail-fast companion.
pub struct HeadlessClipboardCapabilityState;

#[runtime]
impl NativeActor for HeadlessClipboardCapability {
    type State = HeadlessClipboardCapabilityState;
    type Config = ();

    const NAMESPACE: &'static str = "aether.clipboard";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessClipboardCapabilityState, BootError> {
        Ok(HeadlessClipboardCapabilityState)
    }

    #[handler::single]
    fn on_get_text(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: GetClipboardText,
    ) -> GetClipboardTextResult {
        GetClipboardTextResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    #[handler::single]
    fn on_set_text(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetClipboardText,
    ) -> SetClipboardTextResult {
        SetClipboardTextResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use aether_data::MailboxId;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::{MailId, Source};
    use aether_substrate::testing::test_mailer_and_rx;

    #[test]
    fn fail_fast_companion_err_replies_to_get_and_set() {
        let (mailer, _rx) = test_mailer_and_rx();
        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = HeadlessClipboardCapabilityState;

        assert!(matches!(
            HeadlessClipboardCapability::on_get_text(&mut state, &mut ctx, GetClipboardText),
            GetClipboardTextResult::Err { error } if error == UNAVAILABLE_ERROR
        ));
        assert!(matches!(
            HeadlessClipboardCapability::on_set_text(
                &mut state,
                &mut ctx,
                SetClipboardText {
                    text: "ignored".to_owned(),
                },
            ),
            SetClipboardTextResult::Err { error } if error == UNAVAILABLE_ERROR
        ));
    }
}
