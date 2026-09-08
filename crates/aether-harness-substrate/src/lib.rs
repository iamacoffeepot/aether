//! `SubstrateHarness` — the in-process harness harness (ADR-0067, issue #3765).
//!
//! Two driver modes share [`chassis::SubstrateHarnessChassis`]:
//!
//! - **Binary (`aether-chassis-harness`'s `src/bin/substrate-harness.rs`)**
//!   — runs the chassis events loop on the main thread blocking on
//!   `events_rx.recv()`. Driven by the `aether-mcp` harness through the
//!   forward-model RPC (the substrate hosts `RpcServerCapability`).
//! - **In-process ([`SubstrateHarness`] struct)** — substrate state is owned
//!   by the test thread; mail goes through the same sinks + control
//!   plane but replies route to a `RecordingBackend` loopback instead
//!   of a socket. Rust integration tests link this directly via
//!   `aether_harness_substrate::SubstrateHarness`.
//!
//! The harness boots basics only — trace dispatch, the harness cap,
//! lifecycle, the deterministic synthetic window, the observer mailbox — and
//! each test composes the caps its scenario needs on the builder (issue
//! #3764). GPU capture support plugs in through the [`FrameHook`] hook
//! factory (ADR-0161) from `aether-harness-substrate-capture`, which boots
//! the pumped `aether.render` slot, so this crate never depends on
//! aether-render or wgpu.
//!
//! # Driving one
//!
//! A scenario is a labelled sequence of [`HarnessOp`]s handed to
//! [`SubstrateHarness::execute`], which returns an [`ExecutionResult`] you
//! read back by label. This example is the one `CLAUDE.md` points at, and it
//! runs — so the vocabulary in the instructions cannot drift off the API:
//!
//! ```
//! use aether_actor::Addressable;
//! use aether_harness_substrate::{HarnessOp, SubstrateHarness};
//! use aether_window::{
//!     CreateWindow, CreateWindowResult, ListWindows, ListWindowsResult, WindowCapability, WindowMode,
//!     WindowSizeRequest, WindowSpec,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut harness = SubstrateHarness::builder().size(320, 240).build()?;
//!
//! let spec = WindowSpec {
//!     name: "main".to_owned(),
//!     title: "example".to_owned(),
//!     mode: WindowMode::Windowed,
//!     size: Some(WindowSizeRequest { width: 320, height: 240 }),
//! };
//!
//! let result = harness.execute(vec![
//!     ("open", HarnessOp::send_and_await_reply(WindowCapability::NAMESPACE, &CreateWindow { spec })),
//!     ("warm", HarnessOp::advance(2)),
//!     ("windows", HarnessOp::send_and_await_reply(WindowCapability::NAMESPACE, &ListWindows)),
//! ])?;
//!
//! assert!(matches!(result.reply::<CreateWindowResult>("open")?, CreateWindowResult::Ok { .. }));
//!
//! let ListWindowsResult::Ok { windows } = result.reply::<ListWindowsResult>("windows")? else {
//!     panic!("the window created above should be listed");
//! };
//! assert_eq!(windows.len(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! The other ops compose the same way: [`HarnessOp::send_and_settle`] waits
//! for a whole causal chain rather than one reply,
//! [`HarnessOp::poll_until`] re-probes to a wall-clock budget for an effect
//! no chain here can settle, and [`HarnessOp::capture`] /
//! [`HarnessOp::capture_with_mails`] read a frame back as PNG bytes through
//! [`ExecutionResult::captured`] — those two need the render hook wired by
//! `aether-harness-substrate-capture`'s `RenderHarnessBuilderExt::with_render`
//! and a wgpu adapter, which is why they are not in the example above.

pub mod chassis;
mod diagnostics;
mod execute;
mod harness;
#[cfg(test)]
mod mail_latency;
pub mod perf;
mod poll_config;
pub mod pump_stats;
pub mod test_helpers;

pub use chassis::{
    CaptureOutcome, ComposeFn, FrameHook, RenderHookWiring, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
    SubstrateHarnessBuild, SubstrateHarnessChassis, SubstrateHarnessEnv, WORKERS, substrate_harness_observer_mailbox,
};
pub use execute::{
    DEFAULT_POLL_BUDGET, DEFAULT_TICK_DELTA_MICROS, ExecutionError, ExecutionResult, HarnessActor, HarnessOp,
    HarnessOutput, PollObserver,
};
pub use harness::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, HookFactory, SubstrateHarness, SubstrateHarnessBuilder, SubstrateHarnessError,
};
pub use poll_config::{PollConfig, PollConfigLayer, PollOverlay};
pub use pump_stats::PumpStats;
