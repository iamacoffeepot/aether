//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod chassis;
mod cli;
mod driver;
mod executor;
mod mirror;
mod source;

pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use executor::ExecutorShell;
pub use mirror::{GithubMirrorConfig, ProjectionShell};
pub use source::SourceShell;
