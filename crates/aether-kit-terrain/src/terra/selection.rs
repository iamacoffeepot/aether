//! Pure ordered-selection and semantic mutation planning.

use alloc::{collections::BTreeSet, format, string::String, vec::Vec};

use crate::mark::{Mark, MarkGeometry, MarkMutationError, MarkRef};
use crate::world::{MAX_STAMP_VERTICES, WorldPoint};

use super::{TerraCommandResult, TerraError, WorldDelta};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Selection {
    references: Vec<MarkRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionChange {
    RevisionReplaced { observed: MarkRef },
    Deleted { observed: MarkRef },
}

impl Selection {
    pub(super) fn snapshot(&self) -> Vec<MarkRef> {
        self.references.clone()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.references.is_empty()
    }

    pub(super) fn validate_unique(references: &[MarkRef]) -> Result<(), TerraError> {
        let mut ids = BTreeSet::new();
        for reference in references {
            if !ids.insert(reference.id) {
                return Err(TerraError::DuplicateSelection { id: reference.id });
            }
        }
        Ok(())
    }

    pub(super) fn replace(&mut self, references: Vec<MarkRef>) {
        self.references = references;
    }

    pub(super) fn clear(&mut self) {
        self.references.clear();
    }

    pub(super) fn toggle(&mut self, reference: MarkRef) {
        if let Some(index) = self.references.iter().position(|selected| selected.id == reference.id) {
            self.references.remove(index);
        } else {
            self.references.push(reference);
        }
    }

    pub(super) fn apply(&mut self, change: SelectionChange) {
        match change {
            SelectionChange::RevisionReplaced { observed } => {
                if let Some(selected) = self.references.iter_mut().find(|selected| selected.id == observed.id) {
                    *selected = observed;
                }
            }
            SelectionChange::Deleted { observed } => {
                self.references.retain(|selected| selected.id != observed.id);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PlannedUpdate {
    pub(super) requested: MarkRef,
    pub(super) geometry: Option<MarkGeometry>,
    pub(super) label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlannedDelete {
    pub(super) requested: MarkRef,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct BatchProgress {
    changed: Vec<MarkRef>,
    deleted: Vec<MarkRef>,
}

impl BatchProgress {
    pub(super) fn record_update(&mut self, selection: &mut Selection, observed: MarkRef) {
        selection.apply(SelectionChange::RevisionReplaced { observed });
        self.changed.push(observed);
    }

    pub(super) fn record_delete(&mut self, selection: &mut Selection, observed: MarkRef) {
        selection.apply(SelectionChange::Deleted { observed });
        self.deleted.push(observed);
    }

    pub(super) fn finish(self, selection: &Selection, error: Option<TerraError>) -> TerraCommandResult {
        let snapshot = selection.snapshot();
        match error {
            None => TerraCommandResult::Applied { selection: snapshot, changed: self.changed, deleted: self.deleted },
            Some(error) if self.changed.is_empty() && self.deleted.is_empty() => {
                TerraCommandResult::Rejected { selection: snapshot, error }
            }
            Some(error) => TerraCommandResult::PartiallyApplied {
                selection: snapshot,
                changed: self.changed,
                deleted: self.deleted,
                error,
            },
        }
    }
}

pub(super) fn validate_mark(requested: MarkRef, mark: Option<&Mark>) -> Result<(), TerraError> {
    let Some(mark) = mark else {
        return Err(TerraError::MarkMissing { requested });
    };
    let current = mark.reference();
    if current != requested {
        return Err(TerraError::StaleReference { requested, current });
    }
    Ok(())
}

pub(super) fn validate_batch_marks(marks: &[Mark]) -> Result<(), TerraError> {
    for mark in marks {
        let minimum = match &mark.geometry {
            MarkGeometry::Point(_) => continue,
            MarkGeometry::Path(points) => {
                if (2..=MAX_STAMP_VERTICES).contains(&points.len()) {
                    continue;
                }
                2
            }
            MarkGeometry::Area(points) => {
                if (3..=MAX_STAMP_VERTICES).contains(&points.len()) {
                    continue;
                }
                3
            }
        };
        let kind = match &mark.geometry {
            MarkGeometry::Path(_) => "path",
            MarkGeometry::Area(_) => "area",
            MarkGeometry::Point(_) => unreachable!("points are always valid"),
        };
        let length = match &mark.geometry {
            MarkGeometry::Path(points) | MarkGeometry::Area(points) => points.len(),
            MarkGeometry::Point(_) => unreachable!("points are always valid"),
        };
        return Err(TerraError::MarkMutationRejected {
            requested: Some(mark.reference()),
            error: MarkMutationError::InvalidGeometry {
                reason: format!("{kind} requires {minimum}..={MAX_STAMP_VERTICES} world points; got {length}"),
            },
        });
    }
    Ok(())
}

pub(super) fn plan_move(marks: &[Mark], delta: WorldDelta) -> Result<Vec<PlannedUpdate>, TerraError> {
    if delta == WorldDelta::default() {
        return Err(TerraError::NoChange);
    }
    let mut updates = Vec::with_capacity(marks.len());
    for mark in marks {
        let requested = mark.reference();
        if mark.revision == u32::MAX {
            return Err(TerraError::MarkMutationRejected {
                requested: Some(requested),
                error: MarkMutationError::RevisionExhausted,
            });
        }
        let geometry =
            translate_geometry(&mark.geometry, delta).ok_or(TerraError::CoordinateOverflow { reference: requested })?;
        updates.push(PlannedUpdate { requested, geometry: Some(geometry), label: None });
    }
    Ok(updates)
}

pub(super) fn plan_relabel(marks: &[Mark], label: &str) -> Result<Vec<PlannedUpdate>, TerraError> {
    let mut updates = Vec::new();
    for mark in marks {
        if mark.label == label {
            continue;
        }
        let requested = mark.reference();
        if mark.revision == u32::MAX {
            return Err(TerraError::MarkMutationRejected {
                requested: Some(requested),
                error: MarkMutationError::RevisionExhausted,
            });
        }
        updates.push(PlannedUpdate { requested, geometry: None, label: Some(String::from(label)) });
    }
    if updates.is_empty() {
        return Err(TerraError::NoChange);
    }
    Ok(updates)
}

pub(super) fn plan_delete(marks: &[Mark]) -> Vec<PlannedDelete> {
    marks.iter().map(|mark| PlannedDelete { requested: mark.reference() }).collect()
}

fn translate_geometry(geometry: &MarkGeometry, delta: WorldDelta) -> Option<MarkGeometry> {
    match geometry {
        MarkGeometry::Point(point) => translate_point(*point, delta).map(MarkGeometry::Point),
        MarkGeometry::Path(points) => translate_points(points, delta).map(MarkGeometry::Path),
        MarkGeometry::Area(points) => translate_points(points, delta).map(MarkGeometry::Area),
    }
}

fn translate_points(points: &[WorldPoint], delta: WorldDelta) -> Option<Vec<WorldPoint>> {
    points.iter().copied().map(|point| translate_point(point, delta)).collect()
}

fn translate_point(point: WorldPoint, delta: WorldDelta) -> Option<WorldPoint> {
    Some(WorldPoint {
        x_octimeters: point.x_octimeters.checked_add(delta.x_octimeters)?,
        z_octimeters: point.z_octimeters.checked_add(delta.z_octimeters)?,
    })
}

#[cfg(test)]
mod tests {
    use core::slice::from_ref;

    use super::*;
    use crate::mark::MarkId;

    fn reference(id: u32, revision: u32) -> MarkRef {
        MarkRef { id: MarkId::new(id), revision }
    }

    fn point_mark(id: u32, revision: u32, x: i32, z: i32, label: &str) -> Mark {
        Mark {
            id: MarkId::new(id),
            revision,
            geometry: MarkGeometry::Point(WorldPoint::new(x, z)),
            label: String::from(label),
        }
    }

    #[test]
    fn set_toggle_clear_preserve_caller_order_and_noops() {
        let first = reference(3, 1);
        let second = reference(1, 4);
        let mut selection = Selection::default();
        selection.replace(vec![first, second]);
        assert_eq!(selection.snapshot(), vec![first, second]);

        selection.toggle(first);
        assert_eq!(selection.snapshot(), vec![second]);
        selection.toggle(first);
        assert_eq!(selection.snapshot(), vec![second, first]);
        selection.replace(selection.snapshot());
        assert_eq!(selection.snapshot(), vec![second, first]);
        selection.clear();
        selection.clear();
        assert!(selection.is_empty());
    }

    #[test]
    fn duplicate_ids_reject_the_whole_set() {
        let first = reference(7, 1);
        let duplicate = reference(7, 2);
        assert_eq!(
            Selection::validate_unique(&[first, duplicate]),
            Err(TerraError::DuplicateSelection { id: first.id })
        );
    }

    #[test]
    fn missing_and_stale_references_fail_before_any_plan_exists() {
        let requested = reference(4, 2);
        assert_eq!(validate_mark(requested, None), Err(TerraError::MarkMissing { requested }));
        let current = point_mark(4, 3, 0, 0, "camp");
        assert_eq!(
            validate_mark(requested, Some(&current)),
            Err(TerraError::StaleReference { requested, current: current.reference() })
        );
    }

    #[test]
    fn zero_move_and_matching_relabel_are_no_change() {
        let mark = point_mark(1, 1, 8, 9, "camp");
        assert_eq!(plan_move(from_ref(&mark), WorldDelta::default()), Err(TerraError::NoChange));
        assert_eq!(plan_relabel(&[mark], "camp"), Err(TerraError::NoChange));
    }

    #[test]
    fn coordinate_overflow_rejects_the_complete_move_plan() {
        let first = point_mark(1, 1, 10, 20, "first");
        let overflow = point_mark(2, 1, i32::MAX, 30, "second");
        assert_eq!(
            plan_move(&[first, overflow.clone()], WorldDelta { x_octimeters: 1, z_octimeters: 0 }),
            Err(TerraError::CoordinateOverflow { reference: overflow.reference() })
        );
    }

    #[test]
    fn invalid_geometry_and_revision_exhaustion_reject_before_commit() {
        let invalid = Mark {
            id: MarkId::new(5),
            revision: 1,
            geometry: MarkGeometry::Path(vec![WorldPoint::new(0, 0)]),
            label: String::from("invalid"),
        };
        assert!(matches!(
            validate_batch_marks(&[invalid]),
            Err(TerraError::MarkMutationRejected { error: MarkMutationError::InvalidGeometry { .. }, .. })
        ));

        let exhausted = point_mark(6, u32::MAX, 0, 0, "exhausted");
        assert_eq!(
            plan_relabel(from_ref(&exhausted), "new"),
            Err(TerraError::MarkMutationRejected {
                requested: Some(exhausted.reference()),
                error: MarkMutationError::RevisionExhausted,
            })
        );
    }

    #[test]
    fn multi_mark_plans_are_ordered_and_relabel_skips_matches() {
        let first = point_mark(9, 2, 1, 2, "old");
        let second = point_mark(4, 7, 3, 4, "new");
        let moved = plan_move(&[first.clone(), second.clone()], WorldDelta { x_octimeters: 5, z_octimeters: -2 })
            .expect("move plan");
        assert_eq!(moved[0].requested, first.reference());
        assert_eq!(moved[1].requested, second.reference());
        assert_eq!(moved[0].geometry, Some(MarkGeometry::Point(WorldPoint::new(6, 0))));

        let relabeled = plan_relabel(&[first.clone(), second], "new").expect("relabel plan");
        assert_eq!(relabeled.len(), 1);
        assert_eq!(relabeled[0].requested, first.reference());
        assert_eq!(relabeled[0].label.as_deref(), Some("new"));
    }

    #[test]
    fn revision_replacement_and_delete_removal_keep_selection_order() {
        let first = reference(1, 1);
        let second = reference(2, 3);
        let third = reference(3, 2);
        let mut selection = Selection { references: vec![first, second, third] };
        let updated = reference(2, 4);
        selection.apply(SelectionChange::RevisionReplaced { observed: updated });
        selection.apply(SelectionChange::Deleted { observed: first });
        assert_eq!(selection.snapshot(), vec![updated, third]);
    }

    #[test]
    fn failure_after_one_success_reports_exact_partial_without_rollback() {
        let first = reference(1, 1);
        let second = reference(2, 1);
        let mut selection = Selection { references: vec![first, second] };
        let updated = reference(1, 2);
        let mut progress = BatchProgress::default();
        progress.record_update(&mut selection, updated);
        let result = progress.finish(&selection, Some(TerraError::MarkMissing { requested: second }));
        assert_eq!(
            result,
            TerraCommandResult::PartiallyApplied {
                selection: vec![updated, second],
                changed: vec![updated],
                deleted: Vec::new(),
                error: TerraError::MarkMissing { requested: second },
            }
        );
    }

    #[test]
    fn first_failure_is_rejected_and_delete_progress_is_not_rolled_back() {
        let first = reference(1, 1);
        let second = reference(2, 1);
        let selection = Selection { references: vec![first, second] };
        assert_eq!(
            BatchProgress::default().finish(&selection, Some(TerraError::MarkMissing { requested: first })),
            TerraCommandResult::Rejected {
                selection: vec![first, second],
                error: TerraError::MarkMissing { requested: first },
            }
        );

        let mut selection = selection;
        let mut progress = BatchProgress::default();
        progress.record_delete(&mut selection, first);
        assert_eq!(
            progress.finish(&selection, Some(TerraError::MarkMissing { requested: second })),
            TerraCommandResult::PartiallyApplied {
                selection: vec![second],
                changed: Vec::new(),
                deleted: vec![first],
                error: TerraError::MarkMissing { requested: second },
            }
        );
    }
}
