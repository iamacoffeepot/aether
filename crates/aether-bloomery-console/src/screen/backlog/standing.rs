//! Signature standing as the show route stored it.
//!
//! The coordinator verifies before persisting; the pane reports that
//! stored standing and does not re-verify.

use serde_json::Value;

/// One approval's standing, in the order the show route returned.
#[must_use]
pub fn standing_line(approval: &Value) -> String {
    let Some(object) = approval.as_object() else {
        return "standing  unknown".to_owned();
    };
    if let Some(signed) = object.get("AuthorSignature").and_then(Value::as_object) {
        let signer = signed.get("signer").and_then(Value::as_str).unwrap_or("?");
        return format!("standing  signed  {signer}");
    }
    if let Some(observation) = object.get("ObservationAttestation").and_then(Value::as_object) {
        let source = observation.get("source").and_then(Value::as_str).unwrap_or("auto");
        return format!("standing  auto  {source}");
    }
    if object.contains_key("StageReceipt") {
        return "standing  receipt".to_owned();
    }
    if let Some(provenance) = object.get("provenance") {
        return standing_line(provenance);
    }
    "standing  unknown".to_owned()
}

#[cfg(test)]
mod tests {
    use super::standing_line;
    use serde_json::json;

    #[test]
    fn standing_names_the_stored_verification_not_a_recheck() {
        // The plausible bug: the pane ignores provenance and always paints
        // "signed", so an auto-tier approval looks like an author signature.
        assert_eq!(
            standing_line(&json!({"AuthorSignature": {"signer": "owner", "signature": [1]}})),
            "standing  signed  owner"
        );
        assert_eq!(
            standing_line(&json!({"ObservationAttestation": {"source": "approve_gate"}})),
            "standing  auto  approve_gate"
        );
        assert_eq!(
            standing_line(&json!({"provenance": {"AuthorSignature": {"signer": "owner"}}})),
            "standing  signed  owner"
        );
    }
}
