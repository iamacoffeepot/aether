//! aether-chassis-harness: the harness chassis (ADR-0067 / ADR-0161, issue
//! #3813) — the standalone binary form of the in-process `SubstrateHarness`, a
//! loopback-driven chassis for deterministic operations and test evidence.
//!
//! Issue #5734 put it on the shared `aether-chassis` composition layer beside
//! its sibling chassis crates, so its shape now reads like theirs:
//!
//! - [`chassis`] — [`HarnessChassis`], the `BootableChassis` declaration every
//!   path composes through (boot, `--describe`, `--print-config`).
//! - [`cli`] — [`HarnessCli`], the clap root and the overlays it flattens.
//! - [`env`] — [`HarnessEnv`], the config the binary boots from, resolved off
//!   the argv/env/file source stack, plus the harness's own render-size knob.
//! - [`pump`] — [`HarnessDriver`], the render pump loop `main` runs. The
//!   harness is a passive chassis: `main()` IS the driver, because the
//!   offscreen GPU lives on the thread that pumps the `aether.render` slot
//!   (ADR-0161).
//!
//! The in-process counterpart is `aether_harness_substrate::SubstrateHarness`,
//! which composes per scenario over a hermetic source stack. The two are
//! deliberately separate: a binary wants argv/env/file config, the sweep, and
//! the fatal aborter; a test harness wants none of them.

pub mod chassis;
pub mod cli;
pub mod env;
pub mod pump;

pub use chassis::HarnessChassis;
pub use cli::HarnessCli;
pub use env::{DEFAULT_HEIGHT, DEFAULT_WIDTH, HarnessEnv, RenderSizeConfig};
pub use pump::HarnessDriver;
