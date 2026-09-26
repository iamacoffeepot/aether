//! The bundle of workspace programs (ADR-0237): one module per program, with
//! its input kind beside it.
//!
//! - [`environment`] holds `environment.merge`, the Pure program that places
//!   the imported toolchain directory in the imported base userland and
//!   declares the [`aether_workspace::Environment`] the result is
//!   (decision 3).
//!
//! The cargo proofs that run in that environment join this bundle as a
//! `proof` module beside `environment`.

pub mod environment;

aether_actor::export!(public = [environment::EnvironmentMerge], generators = [aether_bloomery_bundle::bundle]);
