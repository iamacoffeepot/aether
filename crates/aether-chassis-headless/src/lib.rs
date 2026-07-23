//! aether-chassis-headless: the headless chassis (ADR-0035/ADR-0073,
//! issue #3811). Std-timer driven, no GPU, no window; replies `Err` to
//! capture / window-mode kinds — desktop-only operations this
//! deployment doesn't support. Produces the `aether-substrate-headless`
//! binary over the shared `aether-chassis` composition layer.

pub mod chassis;
pub mod cli;
pub mod driver;

pub use chassis::{HeadlessChassis, HeadlessEnv};
pub use cli::HeadlessCli;

pub use aether_chassis::autoload::AutoloadComponent;
