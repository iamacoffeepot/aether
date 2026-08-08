//! The outward projection: a self-contained [`ViewDocument`] an adapter can
//! render without querying back into the store (ADR-0149 §The boundary).

use super::Snapshot;
use crate::digest::Digest;
use crate::ids::WorkpieceId;
use crate::port::{BloomView, MemberView, PendingDecisionView, ViewDocument};
use crate::values::Question;

/// Assemble a self-contained [`ViewDocument`] from a snapshot — the pure
/// `Snapshot -> ViewDocument` projection the reconcile port pushes outward
/// (ADR-0149 §The boundary, as amended by [#3471]). Every field an adapter
/// renders rides on the returned document, so the adapter never queries back
/// into the store. Pure: reads the snapshot, allocates a document, mutates
/// nothing.
///
/// Each [`BloomRecord`](crate::BloomRecord) becomes a [`BloomView`] (its sealed-spec id, status,
/// and successor), and each sealed [`crate::Membership`] a [`MemberView`]
/// carrying the member's scope revision, approval evidence, — matched by
/// workpiece from the record's accumulated claims — its resolution claim once
/// integrated (`None` until then), — matched by workpiece from the
/// [`Question`] each open hold resolves to — its pending-decision hold (`None`
/// when the member is not held), and its wedge if it has stopped dispatching
/// for good (`None` while it is still working).
///
/// `resolve_question` resolves an open hold's question digest to its
/// [`Question`] bytes, the same injected read-only resolver
/// [`grade`](crate::study_report::grade) uses for study records: the reducer's
/// snapshot holds question *digests*, not the rendered prompt/options or the
/// member the hold binds to, so a snapshot-only signature could carry neither.
/// A hold whose bytes the resolver cannot read (a caller with no artifact
/// access, e.g. the live-query path) surfaces no `pending_decision` on its
/// member, exactly as an unresolvable study record contributes no cost to a
/// grade.
///
/// [#3471]: https://github.com/iamacoffeepot/aether/issues/3471
#[must_use]
pub fn view_of(snapshot: &Snapshot, resolve_question: impl Fn(&Digest) -> Option<Question>) -> ViewDocument {
    let blooms = snapshot
        .blooms
        .values()
        .map(|record| {
            // Resolve each open hold once, then bind it to the member it names —
            // a parked question raises one hold per member, so the map is small.
            let held: Vec<(WorkpieceId, PendingDecisionView)> = record
                .holds
                .iter()
                .filter_map(|digest| {
                    let question = resolve_question(digest)?;
                    Some((
                        question.workpiece.clone(),
                        PendingDecisionView {
                            question: *digest,
                            stage: question.stage,
                            prompt: question.prompt,
                            options: question.options,
                            blocked: question.blocked,
                        },
                    ))
                })
                .collect();
            let members = record
                .spec
                .members()
                .iter()
                .map(|member| MemberView {
                    workpiece: member.workpiece.clone(),
                    scope_revision: member.scope_revision,
                    approval: member.approval.clone(),
                    resolution: record.claims.get(&member.workpiece).cloned(),
                    pending_decision: held
                        .iter()
                        .find(|(workpiece, _)| *workpiece == member.workpiece)
                        .map(|(_, view)| view.clone()),
                    wedge: record.wedged.get(&member.workpiece).copied(),
                })
                .collect();
            BloomView { id: record.spec.id(), status: record.status, superseded_by: record.superseded_by, members }
        })
        .collect();
    ViewDocument { mainline: snapshot.mainline, blooms }
}
