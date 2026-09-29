//! Pooled forwarding runtime for one synthetic window endpoint.

use aether_actor::runtime;
use aether_substrate::actor::native::{NativeActor, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::SyntheticWindowInstance;
use crate::runtime::instance::{WindowEndpoint, WindowInstanceState};

#[runtime(handler_set(WindowEndpoint))]
impl NativeActor for SyntheticWindowInstance {
    type State = WindowInstanceState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_INSTANCE_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<WindowInstanceState, BootError> {
        Ok(WindowInstanceState)
    }
}

impl WindowEndpoint for SyntheticWindowInstance {
    type State = WindowInstanceState;
}
