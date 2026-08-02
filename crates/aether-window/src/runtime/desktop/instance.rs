//! Pooled forwarding runtime for one desktop window endpoint.

use aether_actor::runtime;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::DesktopWindowInstance;
use crate::runtime::instance::{WindowEndpoint, WindowInstanceState, unwire};

#[runtime(handler_set(WindowEndpoint))]
impl NativeActor for DesktopWindowInstance {
    type State = WindowInstanceState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_INSTANCE_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<WindowInstanceState, BootError> {
        Ok(WindowInstanceState::new())
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        unwire(state);
    }
}

impl WindowEndpoint for DesktopWindowInstance {
    type State = WindowInstanceState;

    fn endpoint(state: &mut Self::State) -> &mut WindowInstanceState {
        state
    }
}
