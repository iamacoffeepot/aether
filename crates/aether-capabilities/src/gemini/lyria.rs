//! Lyria music-generation backend for the `aether.gemini` cap
//! (ADR-0050; Vertex AI Lyria API snapshot 2026-05-20). `model` is one
//! of `lyria-2` / `lyria-3` / `lyria-3-pro`. `seed` and `sample_count`
//! are mutually exclusive. Each clip is a fixed ~30s WAV at 48 kHz;
//! there is no `duration_s` — `sample_count` controls the count, hence
//! the plural `output_paths` on the reply.
//!
//! NOTE (verify-at-impl): the Lyria request/response wire shape is a
//! 2026-05-20 snapshot of the Vertex AI surface. The `model` ids, the
//! `seed` / `sample_count` exclusivity, and the fixed ~30s WAV at
//! 48 kHz output should be re-verified against the live API; the
//! cap-side validation + response parsing below follow the snapshot.
//! `parse_clip_response`
//! reads base64 audio clips out of the Vertex `predictions` array; if
//! the live field names differ, only this parser changes.

use aether_kinds::GeminiError;

/// Supported Lyria models (2026-05-20 snapshot).
pub const MODELS: &[&str] = &["lyria-2", "lyria-3", "lyria-3-pro"];

/// Whether `model` is a supported Lyria model.
#[must_use]
pub fn is_supported(model: &str) -> bool {
    MODELS.contains(&model)
}

/// Every supported model id, for `UnknownModel { supported }`.
#[must_use]
pub fn supported_model_ids() -> Vec<String> {
    MODELS.iter().map(|s| (*s).to_string()).collect()
}

/// Validate a Lyria request before dispatch. `seed` and `sample_count`
/// are mutually exclusive — both set is rejected (the same
/// constraint-validation pattern as Nano Banana's per-model checks).
pub fn validate(model: &str, seed_set: bool, sample_count_set: bool) -> Result<(), GeminiError> {
    if seed_set && sample_count_set {
        return Err(GeminiError::MissingRequiredField {
            model: model.to_string(),
            field: "seed and sample_count are mutually exclusive".to_string(),
        });
    }
    Ok(())
}

/// Parse base64 WAV clips out of a Vertex Lyria response. Returns one
/// `Vec<u8>` per clip. Factored out so a fixture-replay test can lock
/// the response shape (ADR-0050 §4).
pub fn parse_clip_response(json: &str) -> Result<Vec<Vec<u8>>, String> {
    use serde_json::Value;

    let parsed: Value = serde_json::from_str(json).map_err(|e| format!("parse response: {e}"))?;

    let predictions = parsed
        .get("predictions")
        .and_then(Value::as_array)
        .ok_or_else(|| "response missing predictions array".to_string())?;

    let mut clips = Vec::with_capacity(predictions.len());
    for p in predictions {
        let b64 = p
            .get("bytesBase64Encoded")
            .or_else(|| p.get("audioContent"))
            .and_then(Value::as_str)
            .ok_or_else(|| "prediction has no base64 audio field".to_string())?;
        clips.push(super::nanobanana::decode_base64_for_media(b64).map_err(|e| {
            format!("decode clip base64: {e}")
        })?);
    }
    if clips.is_empty() {
        return Err("response carried zero clips".to_string());
    }
    Ok(clips)
}

#[cfg(test)]
mod tests {
    use super::{is_supported, parse_clip_response, supported_model_ids, validate};
    use aether_kinds::GeminiError;

    #[test]
    fn known_models_are_supported() {
        assert!(is_supported("lyria-2"));
        assert!(is_supported("lyria-3"));
        assert!(is_supported("lyria-3-pro"));
        assert!(!is_supported("lyria-bogus"));
        assert_eq!(supported_model_ids().len(), 3);
    }

    #[test]
    fn seed_and_sample_count_both_set_rejected() {
        let err = validate("lyria-3", true, true).expect_err("seed XOR sample_count");
        assert!(matches!(err, GeminiError::MissingRequiredField { .. }));
    }

    #[test]
    fn either_seed_or_sample_count_alone_is_valid() {
        validate("lyria-3", true, false).expect("seed alone is valid");
        validate("lyria-3", false, true).expect("sample_count alone is valid");
        validate("lyria-3", false, false).expect("neither is valid");
    }

    #[test]
    fn parses_multiple_clips() {
        // "Man" -> "TWFu"; two clips.
        let json = r#"{"predictions": [
            {"bytesBase64Encoded": "TWFu"},
            {"bytesBase64Encoded": "TWFu"}
        ]}"#;
        let clips = parse_clip_response(json).expect("two-clip response parses");
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0], b"Man");
    }

    #[test]
    fn empty_predictions_errors() {
        assert!(parse_clip_response(r#"{"predictions": []}"#).is_err());
    }
}
