//! One map-backed [`Correspondence`] double for chassis scenarios.
//!
//! The trait is three methods. Re-deriving a `Mutex<HashMap>` impl next to
//! every unit that needs an in-memory store is how three copies grew; this is
//! the one they reach for. A wrapper that faults `record` still belongs next
//! to the test that injects the fault — that is not a second store.

#![allow(dead_code, reason = "each test binary compiles the whole module and uses only the fixtures it needs")]
#![allow(clippy::unwrap_used, reason = "a fixture that cannot lock its map reports it by panicking")]

use std::collections::HashMap;
use std::sync::Mutex;

use aether_bloomery::{BackendObjectId, Correspondence, CorrespondenceError, Digest};

/// An in-memory two-way digest ↔ backend-object map.
#[derive(Default)]
pub struct MapCorrespondence {
    pairs: Mutex<HashMap<Digest, BackendObjectId>>,
}

impl MapCorrespondence {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Correspondence for MapCorrespondence {
    fn record(&self, digest: &Digest, object: &BackendObjectId) -> Result<(), CorrespondenceError> {
        self.pairs.lock().unwrap().insert(*digest, object.clone());
        Ok(())
    }

    fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
        Ok(self.pairs.lock().unwrap().get(digest).cloned())
    }

    fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
        Ok(self.pairs.lock().unwrap().iter().find_map(|(digest, stored)| (stored == object).then_some(*digest)))
    }
}
