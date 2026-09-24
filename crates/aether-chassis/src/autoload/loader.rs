//! The boot loader: sends the boot components' loads one at a time and
//! reports each answer to the chassis thread that spawned it (issues #6413,
//! #6637).
//!
//! `load_boot_components` spawns [`Autoloader`] at the chassis root after the
//! build and waits on the channel whose sending half rides in
//! [`AutoloaderParams`], one answer per load, each within the boot-load
//! budget. The loader holds nothing else: the chassis thread keeps the
//! components' labels and the RPC bind gate, names any failure or timeout, and
//! opens the gate only after every load has answered `Ok`.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use aether_actor::{DependsOn, actor};
use aether_component::ComponentHostCapability;
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::AutoloadComponent;

/// One load's answer: `Ok` when it loaded, or the component host's error.
pub type LoadAnswer = Result<(), String>;

/// Composer-supplied construction input: the boot components, in load order,
/// and the channel each answer is reported on.
pub struct AutoloaderParams {
    /// The components to load, in manifest order.
    pub components: Vec<AutoloadComponent>,
    /// The sending half the chassis thread waits on.
    pub report: Sender<LoadAnswer>,
}

/// Loads the boot components sequentially: one load in flight, the next sent
/// on the previous `Ok`, and nothing sent after the first `Err`.
pub struct Autoloader {
    /// The components not yet sent, in load order.
    remaining: VecDeque<AutoloadComponent>,
    /// The channel each load's answer is reported on.
    report: Sender<LoadAnswer>,
}

#[actor(instanced, root, depends(ComponentHostCapability))]
impl NativeActor for Autoloader {
    type Config = ();
    type Params = AutoloaderParams;
    const NAMESPACE: &'static str = "aether.chassis.autoload";

    fn init((): (), params: AutoloaderParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let AutoloaderParams { components, report } = params;
        Ok(Self { remaining: components.into(), report })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        self.send_next(ctx);
    }

    #[handler::single]
    fn on_load_result(&mut self, ctx: &mut NativeCtx<'_>, result: LoadResult) {
        match result {
            LoadResult::Ok { path, .. } => {
                tracing::info!(%path, "boot component loaded");
                let _ = self.report.send(Ok(()));
                self.send_next(ctx);
            }
            LoadResult::Err { error } => {
                self.remaining.clear();
                let _ = self.report.send(Err(error));
            }
        }
    }
}

impl Autoloader {
    /// Send the next component's load, if any remain.
    fn send_next<A: DependsOn<ComponentHostCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>) {
        if let Some(component) = self.remaining.pop_front() {
            ctx.send::<ComponentHostCapability>(&component.load_request());
        }
    }
}
