//! The staged overlay one batch mutates before it commits: reads that see
//! the batch's own pending writes, and the commit that folds them into
//! `Inner`.

use rustc_hash::FxHashMap;

use aether_data::{ActorPathForm, CanonicalPath, ErasedActorPath};

use crate::mail::registry::effect::ActivationToken;
use crate::mail::registry::publication::PublicationTable;
use crate::mail::{KindId, MailboxId};

use super::birth::{PendingBirth, RouteContinuation};
use super::kinds::KindSlot;
use super::route::RouteRecord;
use super::{CapturedDisposition, Inner};

pub(super) fn staged_route<'a>(
    staged: &'a FxHashMap<MailboxId, Option<RouteRecord>>,
    inner: &'a Inner,
    id: MailboxId,
) -> Option<&'a RouteRecord> {
    staged.get(&id).map_or_else(|| inner.mailboxes.get(&id), |route| route.as_ref())
}

/// Whether a birth under `path` names a parent the registry holds a record
/// for, in any lifecycle (ADR-0248 §5). A root name has no parent and always
/// passes. The parent's record is found at the fold of the parent's own path
/// and must carry that path as its canonical name, the same two facts
/// `Registry::lineage_order` reads for every ancestor, so a birth this admits
/// leaves that read nothing to miss.
pub(super) fn parent_stands(
    staged: &FxHashMap<MailboxId, Option<RouteRecord>>,
    inner: &Inner,
    path: &CanonicalPath,
) -> bool {
    let Some((parent, parent_id)) = path.parent() else {
        return true;
    };

    staged_route(staged, inner, parent_id).is_some_and(|record| record.canonical_name.as_str() == parent)
}

/// What the registry finds when a birth names `name`.
pub(super) enum BirthName {
    /// The name is canonical and its parent holds a record, or it is a root.
    ParentStands,
    /// The name is canonical and no record stands at its parent.
    ParentUnknown,
    /// The name is a short path, which names no position. A birth renders
    /// `parent/NS:key`, so only a hand-built effect reaches this.
    Short,
}

/// Check the name a birth registers under: a canonical path whose parent
/// stands ([`parent_stands`]).
pub(super) fn birth_name(
    staged: &FxHashMap<MailboxId, Option<RouteRecord>>,
    inner: &Inner,
    name: &ErasedActorPath,
) -> BirthName {
    match name.form() {
        ActorPathForm::Canonical(path) if parent_stands(staged, inner, &path) => BirthName::ParentStands,
        ActorPathForm::Canonical(_) => BirthName::ParentUnknown,
        ActorPathForm::Short { .. } => BirthName::Short,
    }
}

pub(super) fn staged_kind<'a>(
    staged: &'a FxHashMap<KindId, KindSlot>,
    inner: &'a Inner,
    id: KindId,
) -> Option<&'a KindSlot> {
    staged.get(&id).or_else(|| inner.kinds.get(&id))
}

pub(super) fn staged_publications<'a>(staged: Option<&'a PublicationTable>, inner: &'a Inner) -> &'a PublicationTable {
    staged.unwrap_or(&inner.publications)
}

pub(super) fn commit_staged(
    inner: &mut Inner,
    routes: FxHashMap<MailboxId, Option<RouteRecord>>,
    kinds: FxHashMap<KindId, KindSlot>,
    pending: FxHashMap<MailboxId, Option<ActivationToken>>,
    publications: Option<PublicationTable>,
) -> Vec<RouteContinuation> {
    let mut continuations = Vec::new();
    for (id, route) in routes {
        if let Some(route) = route {
            inner.mailboxes.insert(id, route);
        } else {
            inner.mailboxes.remove(&id);
        }
    }
    for (id, slot) in kinds {
        inner.name_index.insert(slot.descriptor.name.clone(), id);
        inner.kinds.insert(id, slot);
    }
    if let Some(publications) = publications {
        inner.publications = publications;
    }
    for (id, token) in pending {
        let unchanged =
            token.is_some_and(|token| inner.pending_births.get(&id).is_some_and(|birth| birth.token == token));
        if unchanged {
            continue;
        }
        if let Some(mut birth) = inner.pending_births.remove(&id) {
            continuations.extend(
                birth
                    .parked
                    .drain(..)
                    .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
            );
        }
        if let Some(token) = token {
            inner.pending_births.insert(id, PendingBirth::placeholder(id, token));
        }
    }
    continuations
}

pub(super) fn staged_pending_token(
    staged: &FxHashMap<MailboxId, Option<ActivationToken>>,
    inner: &Inner,
    id: MailboxId,
) -> Option<ActivationToken> {
    staged.get(&id).copied().unwrap_or_else(|| inner.pending_births.get(&id).map(|birth| birth.token))
}
