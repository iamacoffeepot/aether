//! The boot loader: sends the boot components' loads one at a time and
//! reports the outcome to the chassis thread that spawned it (issue #6413).
//!
//! `load_boot_components` spawns [`Autoloader`] at the chassis root after the
//! build and blocks on the channel whose sending half rides in
//! [`AutoloaderParams`]. The loader holds nothing else: the chassis thread
//! keeps the RPC bind gate and opens it only after the loader reports
//! success.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use aether_actor::actor;
use aether_component::ComponentHostCapability;
use aether_kinds::LoadResult;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::AutoloadComponent;

/// What the loader reports: the count of components loaded, or the failing
/// entry's message.
pub type LoadReport = Result<usize, String>;

/// Composer-supplied construction input: the boot components, in load order,
/// and the channel the outcome is reported on.
pub struct AutoloaderParams {
    /// The components to load, in manifest order.
    pub components: Vec<AutoloadComponent>,
    /// The sending half the chassis thread waits on.
    pub report: Sender<LoadReport>,
}

/// Loads the boot components sequentially: one load in flight, the next sent
/// on the previous `Ok`, and the first `Err` reported and final.
pub struct Autoloader {
    /// The components not yet sent, in load order.
    remaining: VecDeque<AutoloadComponent>,
    /// The label of the load in flight: its `name`, else `export`, else
    /// `#<index>` for its manifest position.
    in_flight: String,
    /// How many components have answered `Ok`.
    loaded: usize,
    /// The report channel, dropped once the outcome is sent.
    report: Option<Sender<LoadReport>>,
}

#[actor(instanced, root)]
impl NativeActor for Autoloader {
    type Config = ();
    type Params = AutoloaderParams;
    const NAMESPACE: &'static str = "aether.chassis.autoload";

    fn init((): (), params: AutoloaderParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let AutoloaderParams { components, report } = params;
        Ok(Self { remaining: components.into(), in_flight: String::new(), loaded: 0, report: Some(report) })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        self.send_next(ctx);
    }

    #[handler::single]
    fn on_load_result(&mut self, ctx: &mut NativeCtx<'_>, result: LoadResult) {
        match result {
            LoadResult::Ok { path, .. } => {
                tracing::info!(%path, "boot component loaded");
                self.loaded += 1;
                self.send_next(ctx);
            }
            LoadResult::Err { error } => {
                self.remaining.clear();
                self.finish(Err(format!("boot component {}: {error}", self.in_flight)));
            }
        }
    }
}

impl Autoloader {
    /// Send the next component's load, or report success once none remain.
    fn send_next(&mut self, ctx: &mut NativeCtx<'_>) {
        let Some(component) = self.remaining.pop_front() else {
            self.finish(Ok(self.loaded));
            return;
        };
        self.in_flight =
            component.name.clone().or_else(|| component.export.clone()).unwrap_or_else(|| format!("#{}", self.loaded));
        ctx.actor::<ComponentHostCapability>().send(&component.load_request());
    }

    /// Send the outcome and drop the sender, so the report is sent once.
    fn finish(&mut self, outcome: LoadReport) {
        if let Some(report) = self.report.take() {
            let _ = report.send(outcome);
        }
    }
}
