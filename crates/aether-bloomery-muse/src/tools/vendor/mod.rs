//! The vendor view: `vendor.list`, `vendor.read`, and `vendor.grep`, the
//! read-only tools over the `cargo vendor` tree a session's proofs build
//! against.
//!
//! Each is its `tree.*` sibling's body run over the vendor tree its call's
//! [`ProofBound`] cites instead of the session's tree, so it takes the same
//! arguments, returns the same [`Viewed`] text, and leaves the session's tree
//! as it was. Paths are relative to the vendor tree's root, which holds one
//! directory per vendored crate. The hints in a result name the `vendor.*`
//! sibling.

mod grep;
mod list;
mod read;

use aether_bloomery_kinds::{Refusal, Tree};
use aether_bloomery_program::{Async, Env};
use aether_bloomery_workspace_programs::proof::ProofBound;
use aether_data::Ref;

pub use grep::VendorGrep;
pub use list::VendorList;
pub use read::VendorRead;

/// The vendor tree the session's `proofs` cite.
///
/// # Errors
///
/// The [`Refusal`] of a bound the store cannot give.
async fn vendor_root(env: &mut Env<Async>, proofs: Ref<ProofBound>) -> Result<Ref<Tree>, Refusal> {
    Ok(env.read(proofs).await?.vendor())
}
