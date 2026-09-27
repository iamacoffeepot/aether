//! The engine state a decode may consult (ADR-0231 §3, ADR-0238 decision 3).
//!
//! [`Kind::decode_with`](crate::Kind::decode_with) takes a [`DecodeCtx`]. A
//! leaf that needs the engine asks the context through one of its two
//! operations: [`DecodeCtx::resolve_blob`] for a tag-1 `Blob` hash, and
//! [`DecodeCtx::prove_route_covers`] for a `ProtocolPath`. The context holds
//! its blob resolver and published routes in private fields, so no decode
//! reaches either hook except through those operations, and each operation
//! refuses with a named error when the context was not given its hook.

use alloc::sync::Arc;

use super::{BlobResolver, Error};
use crate::blob::{Blob, BlobHash};
use crate::{ErasedActorPath, KindId, ReplyContract, first_contract_break};

/// The engine's published route contracts (ADR-0231 §4): an input to
/// [`DecodeCtx::routes`], never handed to a leaf.
pub trait PublishedRoutes {
    /// The rows the `Live` route standing under exactly `path` published, or
    /// `None` when none stands there.
    fn published_rows(&self, path: &ErasedActorPath) -> Option<Arc<[(KindId, ReplyContract)]>>;
}

/// What one decode may resolve against. [`DecodeCtx::empty`] resolves
/// nothing; the consuming setters add the hooks a caller holds.
pub struct DecodeCtx<'a> {
    blobs: Option<&'a mut dyn BlobResolver>,
    routes: Option<&'a dyn PublishedRoutes>,
}

impl<'a> DecodeCtx<'a> {
    /// A context that resolves no blob and proves no route.
    #[must_use]
    pub fn empty() -> Self {
        Self { blobs: None, routes: None }
    }

    /// This context, resolving tag-1 `Blob` hashes through `blobs`.
    #[must_use]
    pub fn blobs(self, blobs: &'a mut dyn BlobResolver) -> Self {
        Self { blobs: Some(blobs), ..self }
    }

    /// This context, proving `ProtocolPath` coverage against `routes`.
    #[must_use]
    pub fn routes(self, routes: &'a dyn PublishedRoutes) -> Self {
        Self { routes: Some(routes), ..self }
    }

    /// The shared value a tag-1 `Blob` field's hash names.
    ///
    /// # Errors
    ///
    /// [`Error::DetachedBlob`] when this context has no resolver or the
    /// resolver does not supply `hash`.
    pub fn resolve_blob(&mut self, hash: BlobHash) -> Result<Blob, Error> {
        self.blobs.as_deref_mut().map_or(Err(Error::DetachedBlob(hash)), |blobs| blobs.resolve(hash))
    }

    /// Prove that the `Live` route standing under exactly `path` publishes
    /// every one of `rows`, by the ADR-0231 §5 keep rule
    /// ([`first_contract_break`] with `rows` as the predecessor).
    ///
    /// # Errors
    ///
    /// [`Error::ProtocolPathUnchecked`] when this context has no published
    /// routes, [`Error::ProtocolPathUnpublished`] when no `Live` route stands
    /// at `path`, and [`Error::UncoveredProtocolPath`] naming the first row
    /// the route does not publish or answers differently.
    pub fn prove_route_covers(&self, path: &ErasedActorPath, rows: &[(KindId, ReplyContract)]) -> Result<(), Error> {
        let routes = self.routes.ok_or_else(|| Error::ProtocolPathUnchecked { path: path.clone() })?;
        let published =
            routes.published_rows(path).ok_or_else(|| Error::ProtocolPathUnpublished { path: path.clone() })?;

        first_contract_break(rows.iter().copied(), published.iter().copied())
            .map_or(Ok(()), |kind| Err(Error::UncoveredProtocolPath { path: path.clone(), kind }))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    const ASKED: KindId = KindId(1);
    const OTHER: KindId = KindId(2);
    const REPLY: KindId = KindId(10);
    const ANOTHER_REPLY: KindId = KindId(11);

    struct Stub(Vec<(ErasedActorPath, Vec<(KindId, ReplyContract)>)>);

    impl PublishedRoutes for Stub {
        fn published_rows(&self, path: &ErasedActorPath) -> Option<Arc<[(KindId, ReplyContract)]>> {
            self.0.iter().find(|(at, _)| at == path).map(|(_, rows)| rows.as_slice().into())
        }
    }

    fn path(text: &str) -> ErasedActorPath {
        ErasedActorPath::new(text).expect("test setup: a valid path")
    }

    // Catches a reversed `first_contract_break` argument order (which accepts
    // a route publishing fewer rows than asked and refuses one publishing
    // more), a loose reply comparison, and a wrong or missing refusal.
    #[test]
    fn context_proves_route_coverage_and_names_each_refusal() {
        let asked = [(ASKED, ReplyContract::One(REPLY))];
        let stub = Stub(Vec::from([
            (path("test.superset"), Vec::from([(ASKED, ReplyContract::One(REPLY)), (OTHER, ReplyContract::None)])),
            (path("test.missing"), Vec::from([(OTHER, ReplyContract::None)])),
            (path("test.other_reply"), Vec::from([(ASKED, ReplyContract::One(ANOTHER_REPLY))])),
            (path("test.manual"), Vec::from([(ASKED, ReplyContract::Manual)])),
        ]));
        let ctx = DecodeCtx::empty().routes(&stub);

        assert_eq!(ctx.prove_route_covers(&path("test.superset"), &asked), Ok(()));
        for uncovered in ["test.missing", "test.other_reply", "test.manual"] {
            assert_eq!(
                ctx.prove_route_covers(&path(uncovered), &asked),
                Err(Error::UncoveredProtocolPath { path: path(uncovered), kind: ASKED }),
            );
        }
        assert_eq!(
            ctx.prove_route_covers(&path("test.unknown"), &asked),
            Err(Error::ProtocolPathUnpublished { path: path("test.unknown") }),
        );

        let mut empty = DecodeCtx::empty();
        let hash = BlobHash::from_bytes([7; 32]);

        assert_eq!(
            empty.prove_route_covers(&path("test.superset"), &asked),
            Err(Error::ProtocolPathUnchecked { path: path("test.superset") }),
        );
        assert_eq!(empty.resolve_blob(hash).map(|_| ()), Err(Error::DetachedBlob(hash)));
    }
}
