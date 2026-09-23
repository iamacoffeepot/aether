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
