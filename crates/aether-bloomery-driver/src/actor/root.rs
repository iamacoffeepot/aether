//! Bundle roots' addressing identities: hand-written markers.
//!
//! A root is generated inside guest wasm by its bundle export, so no native
//! type describes it. Each marker carries only the addressing the driver
//! needs to send it mail: its namespace is the root's own, and [`Embedded`]
//! is the resolver every loaded component sits under. A root is only ever
//! addressed through `actor_at` with the id a `LoadResult` handed over,
//! never resolved by name.

use aether_actor::{Addressable, Embedded, HandlesKind};
use aether_bloomery_kinds::{Event, Invoke, StatusQuery, Warm};
use aether_bloomery_program::PROGRAM_NAMESPACE;
use aether_bloomery_reactor::REACTOR_NAMESPACE;

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

/// Addressing identity of one loaded reactor bundle root.
///
/// Hand-written because the root is generated inside guest wasm and has no
/// native type. Address it only through `actor_at` with the id from its
/// `LoadResult`; resolving it by its digest name would be name-based
/// addressing, and the load reply already hands the id over.
pub struct ReactorBundleRoot;

impl Addressable for ReactorBundleRoot {
    const NAMESPACE: &'static str = REACTOR_NAMESPACE;
    type Resolver = Embedded;
}

impl HandlesKind<Warm> for ReactorBundleRoot {}

impl HandlesKind<Event> for ReactorBundleRoot {}

impl HandlesKind<StatusQuery> for ReactorBundleRoot {}
