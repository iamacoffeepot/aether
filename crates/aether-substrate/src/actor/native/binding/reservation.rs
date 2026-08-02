//! Parent-local child reservations (ADR-0165) — actor-local bookkeeping that
//! never reaches a global registry.

use std::sync::Arc;

use super::NativeBinding;
use crate::actor::native::spawn::reservation::{ChildReservationKey, LiveChildReservation, ParentReservation};

/// ADR-0165 parent-local child reservations. Every operation holds only this
/// binding's table mutex and never reaches a global registry.
impl NativeBinding {
    /// Reserve one distinct child prototype + instanced-node key in this
    /// parent's local table. Staged and live entries both reject duplicates.
    pub(crate) fn reserve_child(self: &Arc<Self>, key: ChildReservationKey) -> Option<ParentReservation> {
        if !self
            .child_reservations
            .lock()
            .expect("child reservation table poisoned; fail-fast per ADR-0063")
            .reserve(key)
        {
            return None;
        }
        Some(ParentReservation::new(self, key))
    }

    pub(crate) fn reject_child_reservation(&self, key: ChildReservationKey) {
        self.child_reservations.lock().expect("child reservation table poisoned; fail-fast per ADR-0063").reject(key);
    }

    pub(crate) fn promote_child_reservation(&self, key: ChildReservationKey) {
        self.child_reservations.lock().expect("child reservation table poisoned; fail-fast per ADR-0063").promote(key);
    }

    pub(crate) fn release_live_child_reservation(&self, key: ChildReservationKey) {
        self.child_reservations
            .lock()
            .expect("child reservation table poisoned; fail-fast per ADR-0063")
            .release_live(key);
    }

    pub(crate) fn retain_parent_child_reservation(&self, reservation: LiveChildReservation) {
        let previous = self
            .parent_child_reservation
            .lock()
            .expect("parent child reservation slot poisoned; fail-fast per ADR-0063")
            .replace(reservation);
        assert!(previous.is_none(), "a child binding retains exactly one parent-local live reservation");
    }

    /// Hand the retained live lease back to the spawning parent at ordinary
    /// live teardown (ADR-0165: "live teardown releases the resulting
    /// live-child key"). Taking the lease out of its slot is what makes the
    /// release exactly-once — a second close-path call finds `None`, so
    /// `ChildReservationTable::release_live`'s live-state assertion is
    /// unreachable from here. A binding that was never spawned as a staged
    /// child holds nothing and this is a no-op.
    pub(crate) fn release_parent_child_reservation(&self) {
        let retained = self
            .parent_child_reservation
            .lock()
            .expect("parent child reservation slot poisoned; fail-fast per ADR-0063")
            .take();
        // Released outside the slot lock: the lease's `Drop` reaches into the
        // *parent's* reservation table, so the two locks are never nested.
        drop(retained);
    }
}
