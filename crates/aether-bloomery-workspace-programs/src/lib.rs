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
//! - [`vendor`] holds `vendor.cargo`, the Sampled program that runs
//!   `cargo vendor --locked` over a source tree in that environment with the
//!   network on and records the vendor tree `proof.clippy` mounts (decisions 2
//!   and 4).
//! - [`source`] holds `source.select`, the Pure program that takes the
//!   checkout from an imported source image, so a proof can cite it as its
//!   source tree (decision 3).

pub mod environment;
pub mod proof;
pub mod source;
pub mod vendor;

aether_actor::export!(
    public = [environment::EnvironmentMerge, proof::ClippyProof, vendor::CargoVendor, source::SourceSelect],
    generators = [aether_bloomery_bundle::bundle]
);
