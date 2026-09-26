//! The bundle of workspace programs (ADR-0237): one module per program, with
//! its input kind beside it.
//!
//! - [`environment`] holds `environment.merge`, the Pure program that places
//!   the imported toolchain directory in the imported base userland and
//!   declares the [`aether_workspace::Environment`] the result is
//!   (decision 3).
//! - [`proof`] holds `proof.clippy`, the Sampled program that runs clippy over
//!   a source tree in that environment through the workspace and records
//!   whether it passed (decisions 2, 4, and 7).

pub mod environment;
pub mod proof;

aether_actor::export!(
    public = [environment::EnvironmentMerge, proof::ClippyProof],
    generators = [aether_bloomery_bundle::bundle]
);
