//! Native doctor-to-API snapshot kinds. Chassis-local: no guest mails the doctor.
//!
//! HTTP `/view` keeps [`CheckResult`] / [`DoctorReport`] as serde JSON DTOs
//! (empty `divergences` omitted). Mail uses always-positional Schema rows.

use serde::{Deserialize, Serialize};

use super::{CheckResult, DoctorReport};

/// One invariant row on the doctor snapshot wire. Always positional; not a Kind.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheckRow {
    /// [`CheckResult::name`].
    pub name: String,
    /// [`CheckResult::statement`].
    pub statement: String,
    /// [`CheckResult::passed`].
    pub passed: bool,
    /// [`CheckResult::divergences`], including empty.
    pub divergences: Vec<String>,
}

/// Immutable last-pass snapshot the doctor reactor publishes to the REST API.
#[aether_data::kind(name = "aether.bloomery.doctor.latest_report", eq)]
pub struct LatestDoctorReport {
    /// Seed invariant rows, in report order.
    pub checks: Vec<DoctorCheckRow>,
}

impl From<CheckResult> for DoctorCheckRow {
    fn from(check: CheckResult) -> Self {
        Self { name: check.name, statement: check.statement, passed: check.passed, divergences: check.divergences }
    }
}

impl From<DoctorCheckRow> for CheckResult {
    fn from(row: DoctorCheckRow) -> Self {
        Self { name: row.name, statement: row.statement, passed: row.passed, divergences: row.divergences }
    }
}

impl From<DoctorReport> for LatestDoctorReport {
    fn from(report: DoctorReport) -> Self {
        Self { checks: report.checks.into_iter().map(DoctorCheckRow::from).collect() }
    }
}

impl From<LatestDoctorReport> for DoctorReport {
    fn from(mail: LatestDoctorReport) -> Self {
        Self { checks: mail.checks.into_iter().map(CheckResult::from).collect() }
    }
}

#[cfg(test)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::{CheckResult, DoctorCheckRow, DoctorReport, LatestDoctorReport};

    fn dirty(name: &str, divergences: Vec<String>) -> CheckResult {
        CheckResult { name: name.into(), statement: "the property held".into(), passed: false, divergences }
    }

    #[test]
    fn mail_snapshot_roundtrips_empty_and_nonempty_divergences_in_order() {
        // The plausible bug: converting to the mail row drops empty divergences
        // or reorders checks, so /view overlays a different failing set.
        let report = DoctorReport {
            checks: vec![
                dirty("observed_head_equals_daily_head", Vec::new()),
                dirty("claim_refs_name_active_blooms", vec!["refs/bloomery/claims/issue-5175".into()]),
            ],
        };
        let mail = LatestDoctorReport::from(report.clone());
        let bytes = to_vec(&mail).expect("the snapshot encodes");
        let decoded = from_bytes::<LatestDoctorReport>(&bytes).expect("the snapshot decodes");
        assert_eq!(decoded, mail);
        assert_eq!(decoded.checks.len(), 2, "both rows ride the wire");
        assert_eq!(decoded.checks[0], DoctorCheckRow::from(report.checks[0].clone()));
        assert!(decoded.checks[0].divergences.is_empty(), "empty divergences stay present on the wire");
        assert_eq!(decoded.checks[1].divergences, ["refs/bloomery/claims/issue-5175"]);
        assert_eq!(DoctorReport::from(decoded), report);
    }

    #[test]
    fn http_dto_omits_empty_divergences() {
        // The plausible bug: adding Schema to CheckResult stripped
        // skip_serializing_if, so GET /view grows `"divergences":[]` on every pass.
        let check = dirty("observed_head_equals_daily_head", Vec::new());
        let value = serde_json::to_value(&check).expect("the HTTP DTO encodes");
        assert!(value.get("divergences").is_none(), "empty divergences stay omitted: {value}");
        let restored: CheckResult = serde_json::from_value(value).expect("the HTTP DTO decodes");
        assert_eq!(restored, check);
    }
}
