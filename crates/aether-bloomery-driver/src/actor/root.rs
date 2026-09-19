//! Bundle root's addressing identity: a hand-written marker.
//!
//! A root is generated inside guest wasm by its bundle export, so no native
//! type describes it. The marker carries only the addressing the driver
//! needs to send it mail: its namespace is the root's own, and [`Embedded`]
//! is the resolver every loaded component sits under. A root is only ever
//! addressed through `actor_at` with the id a `LoadResult` handed over,
//! never resolved by name.

use aether_actor::{Addressable, Embedded, HandlesKind};
use aether_bloomery_kinds::{BUNDLE_NAMESPACE, Event, Invoke, StatusQuery, Warm};

/// Addressing identity of one loaded bundle root.
///
/// Hand-written because the root is generated inside guest wasm and has no
/// native type. Address it only through `actor_at` with the id from its
/// `LoadResult`; resolving it by its digest name would be name-based
/// addressing, and the load reply already hands the id over.
pub struct BundleRoot;

impl Addressable for BundleRoot {
    const NAMESPACE: &'static str = BUNDLE_NAMESPACE;
    type Resolver = Embedded;
}

impl HandlesKind<Invoke> for BundleRoot {}

impl HandlesKind<Warm> for BundleRoot {}

impl HandlesKind<Event> for BundleRoot {}

impl HandlesKind<StatusQuery> for BundleRoot {}
