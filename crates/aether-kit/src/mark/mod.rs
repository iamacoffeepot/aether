//! Revisioned terrain annotations with stable ids.
//!
//! [`MarkBook`] is a selector-only actor at `aether.kit.mark`. It owns a
//! deterministic in-memory store and exposes create, update, delete, get, and
//! list mail. Every accepted mutation returns a named [`MarkRef`], while
//! rejected mutations are transactional and leave state untouched. The full
//! store and its allocation watermark survive component replacement.

#![allow(clippy::needless_pass_by_value)]

mod kinds;
pub use kinds::*;

use alloc::{collections::BTreeMap, string::String};

use aether_actor::{
    ActorInitError, Manual, OutboundReply, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor,
};

use crate::world::MAX_STAMP_VERTICES;

const INITIAL_MARK_ID: u32 = 1;
const INITIAL_REVISION: u32 = 1;

/// Pure storage core used by [`MarkBook`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkStore {
    marks: BTreeMap<MarkId, Mark>,
    next_id: u32,
}

impl Default for MarkStore {
    fn default() -> Self {
        Self { marks: BTreeMap::new(), next_id: INITIAL_MARK_ID }
    }
}

impl MarkStore {
    fn create(&mut self, geometry: MarkGeometry, label: String) -> MarkCreateResult {
        if let Err(error) = validate_geometry(&geometry) {
            return MarkCreateResult::Rejected { error };
        }

        let Some(next_id) = self.next_id.checked_add(1) else {
            return MarkCreateResult::Rejected { error: MarkMutationError::IdExhausted };
        };
        let id = MarkId::new(self.next_id);
        let mark = Mark { id, revision: INITIAL_REVISION, geometry, label };
        let reference = mark.reference();
        self.marks.insert(id, mark);
        self.next_id = next_id;
        MarkCreateResult::Created { reference }
    }

    fn update(&mut self, id: MarkId, geometry: Option<MarkGeometry>, label: Option<String>) -> MarkUpdateResult {
        let Some(mark) = self.marks.get(&id) else {
            return MarkUpdateResult::NotFound { id };
        };
        if geometry.is_none() && label.is_none() {
            return MarkUpdateResult::Rejected { error: MarkMutationError::EmptyUpdate };
        }
        if let Some(geometry) = &geometry
            && let Err(error) = validate_geometry(geometry)
        {
            return MarkUpdateResult::Rejected { error };
        }
        let Some(revision) = mark.revision.checked_add(1) else {
            return MarkUpdateResult::Rejected { error: MarkMutationError::RevisionExhausted };
        };

        let mark = self.marks.get_mut(&id).expect("mark existence was established before validation");
        if let Some(geometry) = geometry {
            mark.geometry = geometry;
        }
        if let Some(label) = label {
            mark.label = label;
        }
        mark.revision = revision;
        MarkUpdateResult::Updated { reference: mark.reference() }
    }

    fn delete(&mut self, id: MarkId) -> MarkDeleteResult {
        self.marks
            .remove(&id)
            .map_or(MarkDeleteResult::NotFound { id }, |mark| MarkDeleteResult::Deleted { reference: mark.reference() })
    }

    fn get(&self, id: MarkId) -> MarkGetResult {
        MarkGetResult { mark: self.marks.get(&id).cloned() }
    }

    fn list(&self) -> MarkListResult {
        MarkListResult { marks: self.marks.values().cloned().collect() }
    }

    fn snapshot(&self) -> SavedMarks {
        SavedMarks { marks: self.marks.values().cloned().collect(), next_id: self.next_id }
    }

    fn restore(saved: SavedMarks) -> Self {
        Self { marks: saved.marks.into_iter().map(|mark| (mark.id, mark)).collect(), next_id: saved.next_id }
    }
}

fn validate_geometry(geometry: &MarkGeometry) -> Result<(), MarkMutationError> {
    let (kind, length, minimum) = match geometry {
        MarkGeometry::Point(_) => return Ok(()),
        MarkGeometry::Path(points) => ("path", points.len(), 2),
        MarkGeometry::Area(points) => ("area", points.len(), 3),
    };
    if !(minimum..=MAX_STAMP_VERTICES).contains(&length) {
        return Err(MarkMutationError::InvalidGeometry {
            reason: format!("{kind} requires {minimum}..={MAX_STAMP_VERTICES} world points; got {length}"),
        });
    }
    Ok(())
}

/// Actor wrapper around the deterministic terrain-mark store.
pub struct MarkBook {
    store: MarkStore,
}

#[actor]
impl WasmActor for MarkBook {
    const NAMESPACE: &'static str = "aether.kit.mark";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { store: MarkStore::default() })
    }

    #[handler::manual]
    fn on_create(&mut self, ctx: &mut WasmCtx<'_, Manual>, request: MarkCreate) {
        let result = self.store.create(request.geometry, request.label);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    #[handler::manual]
    fn on_update(&mut self, ctx: &mut WasmCtx<'_, Manual>, request: MarkUpdate) {
        let result = self.store.update(request.id, request.geometry, request.label);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    #[handler::manual]
    fn on_delete(&mut self, ctx: &mut WasmCtx<'_, Manual>, request: MarkDelete) {
        let result = self.store.delete(request.id);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    #[handler::manual]
    fn on_get(&mut self, ctx: &mut WasmCtx<'_, Manual>, request: MarkGet) {
        let result = self.store.get(request.id);
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    #[handler::manual]
    fn on_list(&mut self, ctx: &mut WasmCtx<'_, Manual>, _request: MarkList) {
        let result = self.store.list();
        if ctx.reply_target().is_some() {
            ctx.reply(&result);
        }
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind::<SavedMarks>(0, &self.store.snapshot());
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(saved) = prior.decode_kind::<SavedMarks>() {
            self.store = MarkStore::restore(saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::WorldPoint;

    fn point(x: i32, z: i32) -> WorldPoint {
        WorldPoint::new(x, z)
    }

    fn create(store: &mut MarkStore, geometry: MarkGeometry, label: &str) -> MarkRef {
        match store.create(geometry, label.into()) {
            MarkCreateResult::Created { reference } => reference,
            MarkCreateResult::Rejected { error } => panic!("create rejected: {error:?}"),
        }
    }

    #[test]
    fn geometry_boundaries_accept_only_scoped_vertex_counts() {
        for length in [0, 1, MAX_STAMP_VERTICES + 1] {
            let geometry = MarkGeometry::Path(vec![point(0, 0); length]);
            assert!(matches!(validate_geometry(&geometry), Err(MarkMutationError::InvalidGeometry { .. })));
        }
        for length in [0, 1, 2, MAX_STAMP_VERTICES + 1] {
            let geometry = MarkGeometry::Area(vec![point(0, 0); length]);
            assert!(matches!(validate_geometry(&geometry), Err(MarkMutationError::InvalidGeometry { .. })));
        }

        assert!(validate_geometry(&MarkGeometry::Point(point(0, 0))).is_ok());
        assert!(validate_geometry(&MarkGeometry::Path(vec![point(0, 0); 2])).is_ok());
        assert!(validate_geometry(&MarkGeometry::Path(vec![point(0, 0); MAX_STAMP_VERTICES])).is_ok());
        assert!(validate_geometry(&MarkGeometry::Area(vec![point(0, 0); 3])).is_ok());
        assert!(validate_geometry(&MarkGeometry::Area(vec![point(0, 0); MAX_STAMP_VERTICES])).is_ok());
    }

    #[test]
    fn create_allocates_stable_ids_and_rejects_without_mutation() {
        let mut store = MarkStore::default();
        let first = create(&mut store, MarkGeometry::Point(point(1, 2)), "first");
        let second = create(&mut store, MarkGeometry::Point(point(3, 4)), "second");
        assert_eq!(first, MarkRef { id: MarkId::new(1), revision: 1 });
        assert_eq!(second, MarkRef { id: MarkId::new(2), revision: 1 });

        let before = store.clone();
        let rejected = store.create(MarkGeometry::Path(vec![point(0, 0)]), "bad".into());
        assert!(matches!(rejected, MarkCreateResult::Rejected { error: MarkMutationError::InvalidGeometry { .. } }));
        assert_eq!(store, before);
    }

    #[test]
    fn update_validates_before_one_revision_bump_and_is_transactional() {
        let mut store = MarkStore::default();
        let created = create(&mut store, MarkGeometry::Point(point(1, 2)), "old");
        let updated_geometry = MarkGeometry::Path(vec![point(2, 3), point(4, 5)]);
        assert_eq!(
            store.update(created.id, Some(updated_geometry.clone()), Some("new".into()),),
            MarkUpdateResult::Updated { reference: MarkRef { id: created.id, revision: 2 } }
        );
        assert_eq!(
            store.get(created.id).mark,
            Some(Mark { id: created.id, revision: 2, geometry: updated_geometry, label: "new".into() })
        );

        for (geometry, label, expected) in [
            (None, None, MarkMutationError::EmptyUpdate),
            (
                Some(MarkGeometry::Area(vec![point(0, 0), point(1, 1)])),
                Some("ignored".into()),
                MarkMutationError::InvalidGeometry { reason: "ignored".into() },
            ),
        ] {
            let before = store.clone();
            let result = store.update(created.id, geometry, label);
            match expected {
                MarkMutationError::EmptyUpdate => {
                    assert_eq!(result, MarkUpdateResult::Rejected { error: MarkMutationError::EmptyUpdate });
                }
                MarkMutationError::InvalidGeometry { .. } => assert!(matches!(
                    result,
                    MarkUpdateResult::Rejected { error: MarkMutationError::InvalidGeometry { .. } }
                )),
                _ => unreachable!(),
            }
            assert_eq!(store, before);
        }
    }

    #[test]
    fn missing_update_and_delete_are_no_ops() {
        let mut store = MarkStore::default();
        let before = store.clone();
        let missing = MarkId::new(40);
        assert_eq!(store.update(missing, None, Some("new".into())), MarkUpdateResult::NotFound { id: missing });
        assert_eq!(store.delete(missing), MarkDeleteResult::NotFound { id: missing });
        assert_eq!(store, before);
    }

    #[test]
    fn delete_returns_last_revision_and_ids_are_not_reused() {
        let mut store = MarkStore::default();
        let first = create(&mut store, MarkGeometry::Point(point(0, 0)), "first");
        assert_eq!(
            store.update(first.id, None, Some("edited".into())),
            MarkUpdateResult::Updated { reference: MarkRef { id: first.id, revision: 2 } }
        );
        assert_eq!(
            store.delete(first.id),
            MarkDeleteResult::Deleted { reference: MarkRef { id: first.id, revision: 2 } }
        );
        let second = create(&mut store, MarkGeometry::Point(point(1, 1)), "second");
        assert_eq!(second.id, MarkId::new(2));
    }

    #[test]
    fn list_is_deterministic_by_mark_id() {
        let mut store = MarkStore::default();
        for label in ["one", "two", "three"] {
            create(&mut store, MarkGeometry::Point(point(0, 0)), label);
        }
        store.delete(MarkId::new(2));
        let ids: Vec<_> = store.list().marks.into_iter().map(|mark| mark.id.get()).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn both_counters_reject_exhaustion_without_mutation() {
        let mut id_exhausted = MarkStore { marks: BTreeMap::new(), next_id: u32::MAX };
        let before = id_exhausted.clone();
        assert_eq!(
            id_exhausted.create(MarkGeometry::Point(point(0, 0)), "nope".into()),
            MarkCreateResult::Rejected { error: MarkMutationError::IdExhausted }
        );
        assert_eq!(id_exhausted, before);

        let mut revision_exhausted = MarkStore::default();
        let reference = create(&mut revision_exhausted, MarkGeometry::Point(point(0, 0)), "max");
        revision_exhausted.marks.get_mut(&reference.id).expect("created mark").revision = u32::MAX;
        let before = revision_exhausted.clone();
        assert_eq!(
            revision_exhausted.update(reference.id, None, Some("nope".into())),
            MarkUpdateResult::Rejected { error: MarkMutationError::RevisionExhausted }
        );
        assert_eq!(revision_exhausted, before);
    }

    #[test]
    fn snapshot_restores_marks_and_allocation_watermark() {
        let mut store = MarkStore::default();
        let first = create(&mut store, MarkGeometry::Point(point(0, 0)), "first");
        create(&mut store, MarkGeometry::Point(point(1, 1)), "second");
        store.delete(first.id);

        let mut restored = MarkStore::restore(store.snapshot());
        assert_eq!(restored, store);
        let next = create(&mut restored, MarkGeometry::Point(point(2, 2)), "third");
        assert_eq!(next.id, MarkId::new(3));
    }
}
