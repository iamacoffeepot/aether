//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod chassis;
mod cli;
mod driver;
mod mirror;
mod mirror_driver;
mod source;

pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use mirror::{GithubMirrorConfig, GithubMirrorOverlay, ProjectionShell};
pub use mirror_driver::{
    DrainTick, MirrorDriverCapability, MirrorDriverState, TOPIC_LANDING_RECEIPT, TOPIC_VIEW_DOCUMENT,
};
pub use source::SourceShell;
