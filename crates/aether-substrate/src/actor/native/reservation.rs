//! Parent-local staged child reservations (ADR-0165).
//!
//! This is deliberately actor-local state. It coordinates uniqueness between
//! one parent's staged and live children without touching the routing or actor
//! registries, and its weak RAII handles do not create a lifetime cascade.

#![allow(dead_code, reason = "ADR-0165 reservation primitives land before production staged spawn wiring")]

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use aether_data::ActorId;

use super::binding::NativeBinding;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChildReservationKey {
    child_type: ActorId,
    child_node: ActorId,
}

impl ChildReservationKey {
    pub(crate) fn new(child_type: ActorId, child_node: ActorId) -> Self {
        Self { child_type, child_node }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ChildReservationState {
    Staged,
    Live,
}

pub struct ChildReservationTable {
    entries: HashMap<ChildReservationKey, ChildReservationState>,
}

impl ChildReservationTable {
    pub(crate) fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub(crate) fn reserve(&mut self, key: ChildReservationKey) -> bool {
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, ChildReservationState::Staged);
        true
    }

    pub(crate) fn reject(&mut self, key: ChildReservationKey) {
        assert_eq!(
            self.entries.get(&key),
            Some(&ChildReservationState::Staged),
            "staged child reservation finalized from a non-staged state"
        );
        self.entries.remove(&key);
    }

    pub(crate) fn promote(&mut self, key: ChildReservationKey) {
        let state = self.entries.get_mut(&key).expect("staged child reservation promoted after it was removed");
        assert_eq!(*state, ChildReservationState::Staged, "staged child reservation promoted from a non-staged state");
        *state = ChildReservationState::Live;
    }

    pub(crate) fn release_live(&mut self, key: ChildReservationKey) {
        assert_eq!(
            self.entries.get(&key),
            Some(&ChildReservationState::Live),
            "live child reservation released from a non-live state"
        );
        self.entries.remove(&key);
    }
}

/// Move-only weak capability for one staged parent-local reservation.
#[must_use = "dropping a ParentReservation rolls back its staged child key"]
pub struct ParentReservation {
    parent: Weak<NativeBinding>,
    key: ChildReservationKey,
    armed: bool,
}

impl ParentReservation {
    pub(crate) fn new(parent: &Arc<NativeBinding>, key: ChildReservationKey) -> Self {
        Self { parent: Arc::downgrade(parent), key, armed: true }
    }

    /// Authoritatively reject the staged birth and release its local key.
    pub(crate) fn reject(mut self) {
        self.armed = false;
        if let Some(parent) = self.parent.upgrade() {
            parent.reject_child_reservation(self.key);
        }
    }

    /// Atomically convert Staged to Live and transfer ownership to the live
    /// teardown capability. Parent loss is a no-op for both capabilities.
    pub(crate) fn promote(mut self) -> LiveChildReservation {
        self.armed = false;
        if let Some(parent) = self.parent.upgrade() {
            parent.promote_child_reservation(self.key);
        }
        LiveChildReservation { parent: self.parent.clone(), key: self.key, armed: true }
    }
}

impl Drop for ParentReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Some(parent) = self.parent.upgrade() {
            parent.reject_child_reservation(self.key);
        }
    }
}

/// Move-only weak capability retaining a live child key until teardown.
#[must_use = "keep the LiveChildReservation until ordinary live teardown"]
pub struct LiveChildReservation {
    parent: Weak<NativeBinding>,
    key: ChildReservationKey,
    armed: bool,
}

impl LiveChildReservation {
    #[cfg(test)]
    fn new(parent: &Arc<NativeBinding>, key: ChildReservationKey) -> Self {
        Self { parent: Arc::downgrade(parent), key, armed: true }
    }
}

impl Drop for LiveChildReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Some(parent) = self.parent.upgrade() {
            parent.release_live_child_reservation(self.key);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test fixture construction is asserted by panic")]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use aether_data::MailboxId;

    use super::*;
    use crate::testing::bare_substrate;

    fn key(suffix: &str) -> ChildReservationKey {
        ChildReservationKey::new(
            ActorId::singleton("test.child.prototype"),
            ActorId::instanced("test.child.prototype", suffix),
        )
    }

    fn binding() -> Arc<NativeBinding> {
        let (_, mailer) = bare_substrate();
        Arc::new(NativeBinding::new_for_test(mailer, MailboxId(17)))
    }

    #[test]
    fn staged_duplicate_is_rejected_and_drop_rolls_back() {
        let parent = binding();
        let key = key("staged");

        let staged = parent.reserve_child(key).expect("first staged reservation wins");
        assert!(parent.reserve_child(key).is_none(), "duplicate staged reservation is rejected");

        drop(staged);
        assert!(parent.reserve_child(key).is_some(), "dropping staged reservation releases the key");
    }

    #[test]
    fn promotion_rejects_duplicates_until_live_teardown() {
        let parent = binding();
        let key = key("live");

        let live = parent.reserve_child(key).expect("staged reservation").promote();
        assert!(parent.reserve_child(key).is_none(), "a live key rejects another staged reservation");

        drop(live);
        assert!(parent.reserve_child(key).is_some(), "live teardown releases the key");
    }

    #[test]
    fn explicit_rejection_releases_only_a_staged_key() {
        let parent = binding();
        let key = key("reject");

        parent.reserve_child(key).expect("staged reservation").reject();
        assert!(parent.reserve_child(key).is_some(), "authoritative rejection releases the staged key");
    }

    #[test]
    fn repeated_finalization_with_a_live_parent_fails_fast() {
        let parent = binding();
        let key = key("exact-once");
        let live = parent.reserve_child(key).expect("staged reservation").promote();
        let duplicate = LiveChildReservation::new(&parent, key);

        drop(live);
        let outcome = catch_unwind(AssertUnwindSafe(|| drop(duplicate)));
        assert!(outcome.is_err(), "a second live release is a state mismatch and must fail fast");
    }

    #[test]
    fn state_mismatch_fails_fast_without_removing_the_valid_reservation() {
        let mut table = ChildReservationTable::new();
        let live_key = key("reject-live");
        assert!(table.reserve(live_key));
        table.promote(live_key);
        assert!(catch_unwind(AssertUnwindSafe(|| table.reject(live_key))).is_err());
        table.release_live(live_key);

        let staged_key = key("release-staged");
        assert!(table.reserve(staged_key));
        assert!(catch_unwind(AssertUnwindSafe(|| table.release_live(staged_key))).is_err());
        table.reject(staged_key);
    }

    #[test]
    fn parent_loss_makes_staged_and_live_finalization_no_ops() {
        let parent = binding();
        let staged = parent.reserve_child(key("lost-staged")).expect("staged reservation");
        let live = parent.reserve_child(key("lost-live")).expect("second staged reservation").promote();

        drop(parent);
        drop(staged);
        drop(live);
    }
}
