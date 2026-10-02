//! The bundle of workspace programs (ADR-0237): one module per program, with
//! its input kind beside it.
//!
//! - [`environment`] holds `environment.merge`, the Pure program that places
//!   the imported toolchain directory in the imported base userland and
//!   declares the [`aether_bloomery_workspace::Environment`] the result is
//!   (decision 3).
//! - [`proof`] holds the proofs, the Sampled tools that format a source
//!   tree with `cargo fmt` and prove it in that environment through the
//!   workspace, returning the formatted tree and whether it passed
//!   (decisions 2, 4, 7, and 12): `proof.clippy`, which checks it with
//!   `cargo clippy`, and `proof.test`, which runs its workspace tests with
//!   the session's test env.
//! - [`vendor`] holds `vendor.cargo`, the Sampled program that runs
//!   `cargo vendor --locked` over a source tree in that environment with the
//!   network on and records the vendor tree the proofs mount (decisions 2
//!   and 4).

pub mod environment;
pub mod proof;
pub mod vendor;

use aether_bloomery_kinds::{Head, OpaqueBytes};

/// The head the operator binds to this bundle, which a caller of its
/// programs, such as a Muse session offering `proof.clippy` and `proof.test`, names.
pub const WORKSPACE_PROGRAMS: Head<OpaqueBytes> = Head::new("workspace-programs");

aether_actor::export!(
    public = [environment::EnvironmentMerge, proof::ClippyProof, proof::TestProof, vendor::CargoVendor],
    generators = [aether_bloomery_program::bundle]
);
