//! The program bundle root's addressing identity: a hand-written marker.
//!
//! A root is generated inside guest wasm by the `bundle_programs` export, so
//! no native type describes it. This marker carries only the addressing the
//! driver needs to send it [`Invoke`]: its namespace is the program root's
//! own, and [`Embedded`] is the resolver every loaded component sits under.
//! It is only ever addressed through `actor_at` with the id a `LoadResult`
//! handed over, never resolved by name.

use aether_actor::{Addressable, Embedded, HandlesKind};
use aether_bloomery_kinds::Invoke;
use aether_bloomery_program::PROGRAM_NAMESPACE;

/// Addressing identity of one loaded program bundle root.
///
/// Hand-written because the root is generated inside guest wasm and has no
/// native type. Address it only through `actor_at` with the id from its
/// `LoadResult`; resolving it by its digest name would be name-based
/// addressing, and the load reply already hands the id over.
pub struct ProgramBundleRoot;

impl Addressable for ProgramBundleRoot {
    const NAMESPACE: &'static str = PROGRAM_NAMESPACE;
    type Resolver = Embedded;
}

impl HandlesKind<Invoke> for ProgramBundleRoot {}
