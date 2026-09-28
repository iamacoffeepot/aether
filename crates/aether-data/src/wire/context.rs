//! The engine state a decode may consult (ADR-0231 §3, ADR-0238 decision 3).
//!
//! [`Kind::decode_with`](crate::Kind::decode_with) takes a [`DecodeCtx`]. A
//! leaf that needs the engine asks the context through one of its three
//! operations: [`DecodeCtx::resolve_blob`] for a tag-1 `Blob` hash,
//! [`DecodeCtx::prove_route_covers`] for a `ProtocolPath`, and
//! [`DecodeCtx::claim_held`] for a held-reply ticket (ADR-0243). The context
//! holds its blob resolver, published routes and held ledger in private
//! fields, so no decode reaches any hook except through those operations, and
//! each operation refuses with a named error when the context was not given
//! its hook.

use alloc::sync::Arc;

use super::{BlobResolver, Error, HeldClaim, HeldLedger};
use crate::blob::{Blob, BlobHash};
use crate::{ErasedActorPath, KindId, ReplyContract};

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
    held: Option<&'a mut dyn HeldLedger>,
}

impl<'a> DecodeCtx<'a> {
    /// A context that resolves no blob, proves no route and claims no held
    /// ticket.
    #[must_use]
    pub fn empty() -> Self {
        Self { blobs: None, routes: None, held: None }
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

    /// This context, claiming held tickets back from `ledger` (ADR-0243).
    #[must_use]
    pub fn held(self, ledger: &'a mut dyn HeldLedger) -> Self {
        Self { held: Some(ledger), ..self }
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

    /// Claim the held ticket a decoded field carries: an obligation to answer
    /// `reply`, handed back by the granted ledger.
    ///
    /// # Errors
    ///
    /// [`Error::HeldUngranted`] when this context has no ledger, and the
    /// ledger's [`Error::HeldUnclaimed`] when it does not hold `ticket` for
    /// `reply`.
    pub fn claim_held(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        self.held.as_deref_mut().map_or(Err(Error::HeldUngranted { reply }), |held| held.claim(ticket, reply))
    }

    /// Prove that the `Live` route standing under exactly `path` publishes
    /// every one of `rows` with the exact same [`ReplyContract`]. Protocol
    /// coverage is exact: replacement compatibility is a separate rule and
    /// does not make a manual row interchangeable with a declared one.
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

        rows.iter()
            .find(|row| !published.contains(row))
            .map_or(Ok(()), |(kind, _)| Err(Error::UncoveredProtocolPath { path: path.clone(), kind: *kind }))
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

    // Catches reversed subset comparison (which accepts a route publishing
    // fewer rows than asked and refuses a superset), a loose reply comparison,
    // substitution of replacement's Manual wildcard, and a wrong or missing
    // first-row refusal.
    #[test]
    fn context_proves_route_coverage_and_names_each_refusal() {
        let asked = [(ASKED, ReplyContract::One(REPLY))];
        let stub = Stub(Vec::from([
            (path("test.superset"), Vec::from([(ASKED, ReplyContract::One(REPLY)), (OTHER, ReplyContract::None)])),
            (path("test.missing"), Vec::from([(OTHER, ReplyContract::None)])),
            (path("test.other_reply"), Vec::from([(ASKED, ReplyContract::One(ANOTHER_REPLY))])),
            (path("test.manual_for_declared"), Vec::from([(ASKED, ReplyContract::Manual)])),
            (path("test.manual_exact"), Vec::from([(ASKED, ReplyContract::Manual)])),
            (path("test.manual_silent"), Vec::from([(ASKED, ReplyContract::None)])),
            (path("test.manual_reply"), Vec::from([(ASKED, ReplyContract::One(REPLY))])),
            (path("test.first_missing"), Vec::from([(OTHER, ReplyContract::None)])),
        ]));
        let ctx = DecodeCtx::empty().routes(&stub);

        assert_eq!(ctx.prove_route_covers(&path("test.superset"), &asked), Ok(()));
        for uncovered in ["test.missing", "test.other_reply", "test.manual_for_declared"] {
            assert_eq!(
                ctx.prove_route_covers(&path(uncovered), &asked),
                Err(Error::UncoveredProtocolPath { path: path(uncovered), kind: ASKED }),
            );
        }

        let manual = [(ASKED, ReplyContract::Manual)];
        assert_eq!(ctx.prove_route_covers(&path("test.manual_exact"), &manual), Ok(()));
        for uncovered in ["test.manual_silent", "test.manual_reply"] {
            assert_eq!(
                ctx.prove_route_covers(&path(uncovered), &manual),
                Err(Error::UncoveredProtocolPath { path: path(uncovered), kind: ASKED }),
            );
        }

        let two_rows = [(ASKED, ReplyContract::One(REPLY)), (OTHER, ReplyContract::None)];
        assert_eq!(
            ctx.prove_route_covers(&path("test.first_missing"), &two_rows),
            Err(Error::UncoveredProtocolPath { path: path("test.first_missing"), kind: ASKED }),
        );
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
