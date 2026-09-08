//! The findings a landed bloom's read filed (ADR-0216 §3), selected for the
//! bloom they came out of.
//!
//! A filing is an ordinary open commission. Nothing on it names a bloom — the
//! only link back is its intent's own derivation, which names the *landing
//! receipt* the read consumed. The bloom's side of that link is its `Study`
//! evidence, whose subject is that same receipt digest, which is why this reads
//! the bloom-filtered journal rather than the live view: the receipt is a
//! recorded fact about the read, not projected state.
//!
//! The match is on the receipt and nothing else. An id prefix would gather every
//! reader filing in the estate onto whichever bloom happened to be open, which
//! is the failure this module exists to not have.

use serde_json::Value;

use crate::dto::{CommissionsView, DigestHex, JournalPage, JournalRecordView};

/// One filing as the bloom's tail renders it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiledRow {
    /// The commission's workpiece id — the `retrospect-…` handle to scope it by.
    pub id: String,
    /// The work order's heading, or empty when the intent carries none.
    pub title: String,
    /// The crate globs the reader said the work would touch.
    pub surface: Vec<String>,
    /// The commission's lifecycle word, straight from the store.
    pub status: String,
}

/// The landing receipt `bloom`'s own read bound its evidence to, from a
/// bloom-filtered journal page.
///
/// `None` until the read has been admitted — an unlanded bloom, a read still in
/// flight, and a page that does not reach back to the record are the same answer
/// here, and all three mean the same thing to the reader: nothing to show yet.
#[must_use]
pub fn read_receipt(page: &JournalPage, bloom: DigestHex) -> Option<DigestHex> {
    // One read per bloom — the binding carries a single attempt — so the first
    // record naming it is the record.
    page.records.iter().find_map(|record| study_receipt(record, bloom))
}

/// The commissions filed by the read of `receipt`, in list order.
///
/// Every filing, whatever became of it: an open one is the pile nobody has read
/// yet, and a cancelled or landed one is the record that somebody did — which is
/// the difference an operator is looking at this section to see. The status word
/// rides on the row rather than filtering it.
#[must_use]
pub fn filings(list: &CommissionsView, receipt: DigestHex) -> Vec<FiledRow> {
    list.commissions
        .iter()
        .filter_map(|head| {
            let filed = head.filed.as_ref().filter(|filed| filed.receipt == receipt)?;

            Some(FiledRow {
                id: head.id.clone(),
                title: filed.title.clone(),
                surface: filed.surface.clone(),
                status: head.status.clone(),
            })
        })
        .collect()
}

/// The receipt digest a `StudyCompleted` record binds to, when the record is
/// this bloom's.
///
/// Reads the decoded event as JSON rather than through a typed `Fact`: the
/// console never owns the coordinator's fact vocabulary, and an unknown variant
/// beside this one must not take the page down.
fn study_receipt(record: &JournalRecordView, bloom: DigestHex) -> Option<DigestHex> {
    let study = record.event.get("fact")?.get("StudyCompleted")?;
    if digest_at(study, "bloom")? != bloom {
        return None;
    }

    digest_at(study.get("evidence")?, "subject")
}

fn digest_at(value: &Value, field: &str) -> Option<DigestHex> {
    serde_json::from_value(value.get(field)?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::{FiledRow, filings, read_receipt};
    use crate::dto::{
        CommissionHeadView, CommissionsView, DigestHex, FiledFindingView, JournalPage, JournalRecordView,
    };
    use serde_json::json;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn study_record(bloom: DigestHex, receipt: DigestHex) -> JournalRecordView {
        JournalRecordView {
            sequence: 7,
            event: json!({
                "idempotency_key": "study-recorded:dispatch-1",
                "fact": {"StudyCompleted": {
                    "bloom": bloom.as_hex(),
                    "passed": true,
                    "evidence": {"subject": receipt.as_hex(), "kind": "StudyRecord", "detail": digest(9).as_hex()},
                }},
            }),
            ..JournalRecordView::default()
        }
    }

    fn filing(id: &str, receipt: DigestHex, status: &str) -> CommissionHeadView {
        CommissionHeadView {
            id: id.to_owned(),
            status: status.to_owned(),
            filed: Some(FiledFindingView {
                receipt,
                title: format!("{id} title"),
                surface: vec!["crates/aether-bloomery/**".to_owned()],
            }),
            ..CommissionHeadView::default()
        }
    }

    #[test]
    fn only_this_blooms_read_is_selected() {
        // The plausible bug: the pile is gathered by the `retrospect-` id prefix
        // (or by "has a derivation at all"), so every reader filing in the
        // estate renders under whichever bloom the operator happened to open,
        // and each bloom claims work it never produced.
        let (mine, theirs) = (digest(0x11), digest(0x22));
        let list = CommissionsView {
            commissions: vec![
                filing("retrospect-aaaaaaaaaaaa", mine, "open"),
                filing("retrospect-bbbbbbbbbbbb", theirs, "open"),
                CommissionHeadView { id: "issue-5809".to_owned(), status: "open".to_owned(), ..Default::default() },
            ],
        };

        assert_eq!(
            filings(&list, mine),
            vec![FiledRow {
                id: "retrospect-aaaaaaaaaaaa".to_owned(),
                title: "retrospect-aaaaaaaaaaaa title".to_owned(),
                surface: vec!["crates/aether-bloomery/**".to_owned()],
                status: "open".to_owned(),
            }],
        );
    }

    #[test]
    fn the_receipt_comes_from_this_blooms_own_study_record() {
        // The plausible bug: the walk takes the first `StudyCompleted` on the
        // page regardless of which bloom it names, so a bloom with no read of
        // its own borrows a neighbour's receipt and shows the neighbour's pile.
        let (mine, receipt) = (digest(1), digest(0x11));
        let page = JournalPage {
            records: vec![study_record(digest(2), digest(0x22)), study_record(mine, receipt)],
            ..JournalPage::default()
        };

        assert_eq!(read_receipt(&page, mine), Some(receipt));
        assert_eq!(read_receipt(&page, digest(3)), None, "a bloom whose read is not on the page shows nothing");
    }
}
