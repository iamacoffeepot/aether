//! The staged overlay one batch mutates before it commits: reads that see
//! the batch's own pending writes, and the commit that folds them into
//! `Inner`.

use rustc_hash::FxHashMap;

use crate::mail::registry::effect::ActivationToken;
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

pub(super) fn staged_kind<'a>(
    staged: &'a FxHashMap<KindId, KindSlot>,
    inner: &'a Inner,
    id: KindId,
) -> Option<&'a KindSlot> {
    staged.get(&id).or_else(|| inner.kinds.get(&id))
}

pub(super) fn commit_staged(
    inner: &mut Inner,
    routes: FxHashMap<MailboxId, Option<RouteRecord>>,
    kinds: FxHashMap<KindId, KindSlot>,
    pending: FxHashMap<MailboxId, Option<ActivationToken>>,
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
