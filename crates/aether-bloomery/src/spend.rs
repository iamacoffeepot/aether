//! The window spend projection (ADR-0192).
//!
//! [`measure`] is a pure read over a [`Snapshot`]: it folds each bloom's
//! admitted [`EvidenceKind::StudyRecord`] entries into a
//! [`crate::SpendWindow`] by summing [`crate::StudyCost::cost_micro_usd`] on the
//! resolved artifacts. It re-derives nothing — the priced column is the
//! figure the sealed table produced at intake, band selection included — so
//! the governor and the ledger share one accounting path.
//!
//! The same injected `Fn(&Digest) -> Option<StudyRecord>` resolver
//! [`crate::grade`] takes: the evidence log holds
//! digests, not columns. An unresolvable record, or a resolved one that does
//! not grade its evidence's subject or name its own bloom, contributes zero
//! and raises the unaccounted count. A record whose priced column is zero
//! raises the unpriced count, so a fleet nobody has authored rates for is
//! distinguishable from a cheap one.

use crate::digest::Digest;
use crate::reduce::Snapshot;
use crate::values::{EvidenceKind, SpendWindow, StudyRecord};

/// Measure the window's spend from every bloom's admitted study records.
///
/// Pure: reads the snapshot and resolves study-record bytes through `source`,
/// mutates nothing. The host names the window by writing
/// [`SpendWindow::label`] on the returned value —
/// this crate has no clock and will not invent a day.
#[must_use]
pub fn measure(snapshot: &Snapshot, source: impl Fn(&Digest) -> Option<StudyRecord>) -> SpendWindow {
    let mut spend = SpendWindow::default();
    for (id, record) in &snapshot.blooms {
        let mut bloom_total = 0u64;
        for evidence in &record.evidence {
            if evidence.kind != EvidenceKind::StudyRecord {
                continue;
            }
            match source(&evidence.detail) {
                Some(study) if study.grades(&evidence.subject) && study.bloom == *id => {
                    let cost = study.cost.cost_micro_usd;
                    bloom_total = bloom_total.saturating_add(cost);
                    if cost == 0 {
                        spend.unpriced_records = spend.unpriced_records.saturating_add(1);
                    }
                }
                _ => spend.unaccounted_dispatches = spend.unaccounted_dispatches.saturating_add(1),
            }
        }
        spend.per_bloom.insert(*id, bloom_total);
        spend.total_micro_usd = spend.total_micro_usd.saturating_add(bloom_total);
    }
    spend
}

#[cfg(test)]
mod tests {
    use alloc::collections::{BTreeMap, BTreeSet};

    use super::measure;
    use crate::digest::Digest;
    use crate::ids::BloomId;
    use crate::reduce::{BloomRecord, BloomStatus, Snapshot};
    use crate::values::{
        BloomDraft, Evidence, EvidenceKind, SpendCeiling, SpendQuiesce, SpendWindow, StageCatalog, StudyCost,
        StudyRecord,
    };

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn bloom_id(seed: u8) -> BloomId {
        BloomId(digest(seed))
    }

    fn record(bloom_id: BloomId, subject: Digest, cost_micro_usd: u64) -> StudyRecord {
        StudyRecord { bloom: bloom_id, subject, cost: StudyCost { cost_micro_usd, ..StudyCost::default() } }
    }

    fn snapshot_with(bloom: BloomId, evidence: Vec<Evidence>) -> Snapshot {
        let record = BloomRecord {
            spec: BloomDraft::default().seal(),
            stage_catalog: StageCatalog::line(),
            status: BloomStatus::Sealed,
            claims: BTreeMap::new(),
            evidence,
            holds: BTreeSet::new(),
            progress: BTreeMap::new(),
            wedged: BTreeMap::new(),
            dispatches: BTreeMap::new(),
            integration: None,
            aggregate_rolls: 0,
            aggregate_verify_rolls: 0,
            landing_rolls: 0,
            resolved_head: None,
            review_park: None,
            verify_proofs: BTreeMap::new(),
            verify_reuses: Vec::new(),
            aggregate_fault: None,
            composition_findings: Vec::new(),
            adjudications: Vec::new(),
            operator_repairs: Vec::new(),
            operator_hold: None,
            deferred_dispatches: BTreeSet::new(),
            dependencies: Vec::new(),
            superseded_by: None,
        };
        let mut snapshot = Snapshot::default();
        snapshot.blooms.insert(bloom, record);
        snapshot
    }

    // The plausible bug: an unresolvable study digest is treated as spend,
    // so a missing artifact can trip a ceiling the ledger never recorded.
    #[test]
    fn an_unresolvable_record_raises_the_unaccounted_count_not_the_total() {
        let bloom = bloom_id(1);
        let snapshot = snapshot_with(
            bloom,
            vec![Evidence { subject: digest(2), kind: EvidenceKind::StudyRecord, detail: digest(3) }],
        );

        let spend = measure(&snapshot, |_| None);
        assert_eq!(spend.total_micro_usd, 0, "an unresolvable record must not become spend");
        assert_eq!(spend.unaccounted_dispatches, 1, "the gap is counted, not folded into the total");
        assert_eq!(spend.unpriced_records, 0);
        assert_eq!(spend.per_bloom.get(&bloom), Some(&0));
    }

    // The plausible bug: a record that names a different bloom, or that does
    // not grade its evidence's subject, is summed as if it belonged — a second
    // accounting path that would let a planted artifact move the governor.
    #[test]
    fn a_mismatched_record_is_unaccounted_rather_than_summed() {
        let bloom = bloom_id(1);
        let subject = digest(2);
        let snapshot =
            snapshot_with(bloom, vec![Evidence { subject, kind: EvidenceKind::StudyRecord, detail: digest(3) }]);

        let spend = measure(&snapshot, |_| Some(record(bloom_id(9), subject, 1_000_000)));
        assert_eq!(spend.total_micro_usd, 0);
        assert_eq!(spend.unaccounted_dispatches, 1);

        let spend = measure(&snapshot, |_| Some(record(bloom, digest(8), 1_000_000)));
        assert_eq!(spend.total_micro_usd, 0);
        assert_eq!(spend.unaccounted_dispatches, 1);
    }

    // The plausible bug: a model the table priced at nothing is treated as a
    // cheap fleet (total stays zero, unpriced stays zero), so a ceiling never
    // trips against a fleet nobody has authored rates for and nobody can tell.
    #[test]
    fn a_zero_priced_record_raises_the_unpriced_count() {
        let bloom = bloom_id(1);
        let subject = digest(2);
        let snapshot =
            snapshot_with(bloom, vec![Evidence { subject, kind: EvidenceKind::StudyRecord, detail: digest(3) }]);

        let spend = measure(&snapshot, |_| Some(record(bloom, subject, 0)));
        assert_eq!(spend.total_micro_usd, 0);
        assert_eq!(spend.unpriced_records, 1);
        assert_eq!(spend.unaccounted_dispatches, 0);
        assert_eq!(spend.per_bloom.get(&bloom), Some(&0));
    }

    // The plausible bug: two resolved records of the same bloom are not
    // summed, so a bloom that spent twice is reported at the last record
    // only and the window axis under-counts.
    #[test]
    fn resolved_records_that_grade_their_subject_are_summed() {
        let bloom = bloom_id(1);
        let first = digest(2);
        let second = digest(3);
        let snapshot = snapshot_with(
            bloom,
            vec![
                Evidence { subject: first, kind: EvidenceKind::StudyRecord, detail: digest(4) },
                Evidence { subject: second, kind: EvidenceKind::StudyRecord, detail: digest(5) },
            ],
        );
        let spend = measure(&snapshot, |asked| {
            if *asked == digest(4) {
                Some(record(bloom, first, 7))
            } else if *asked == digest(5) {
                Some(record(bloom, second, 11))
            } else {
                None
            }
        });
        assert_eq!(spend.total_micro_usd, 18);
        assert_eq!(spend.per_bloom.get(&bloom), Some(&18));
        assert_eq!(spend.unaccounted_dispatches, 0);
        assert_eq!(spend.unpriced_records, 0);
    }

    // The plausible bug: the two axes are collapsed into one comparison, so a
    // bloom under its own ceiling still closes the door on a window that is
    // under, or a window crossing names a bloom instead.
    #[test]
    fn the_window_axis_names_the_window_and_the_bloom_axis_names_the_first_bloom() {
        let early = bloom_id(1);
        let late = bloom_id(2);
        let mut per_bloom = BTreeMap::new();
        per_bloom.insert(early, 4);
        per_bloom.insert(late, 10);
        let spend = SpendWindow {
            label: String::from("bloomery/daily/2026-08-14"),
            total_micro_usd: 14,
            per_bloom,
            unaccounted_dispatches: 0,
            unpriced_records: 0,
        };

        let window = SpendCeiling { window_micro_usd: Some(10), bloom_micro_usd: Some(8) };
        assert_eq!(
            window.quiesce(&spend),
            Some(SpendQuiesce::Window {
                window: String::from("bloomery/daily/2026-08-14"),
                spent_micro_usd: 14,
                ceiling_micro_usd: 10,
            }),
        );

        let bloom_only = SpendCeiling { window_micro_usd: None, bloom_micro_usd: Some(8) };
        assert_eq!(
            bloom_only.quiesce(&spend),
            Some(SpendQuiesce::Bloom {
                window: String::from("bloomery/daily/2026-08-14"),
                bloom: late,
                spent_micro_usd: 10,
                ceiling_micro_usd: 8,
            }),
        );

        let open = SpendCeiling { window_micro_usd: Some(20), bloom_micro_usd: Some(12) };
        assert_eq!(open.quiesce(&spend), None);
        assert_eq!(SpendCeiling::default().quiesce(&spend), None);
    }
}
