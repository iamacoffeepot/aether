//! Nano Banana image-generation backend for the `aether.gemini` cap
//! (ADR-0050). `POST` to `generativelanguage.googleapis.com` with
//! `GEMINI_API_KEY`. Per-model validation of `aspect_ratio` /
//! `image_size` / reference-path counts runs *before* any HTTP dispatch
//! (the `ModelShape` table below), returning the matching `GeminiError`
//! variant. Response parsing is factored into
//! [`parse_image_response`] so a fixture-replay test can lock the
//! response shape (ADR-0050 §4).

use super::{AspectRatio, GeminiError, ImageSize};

/// The three Nano Banana models the cap accepts, with their per-model
/// support shape. Drives both `UnknownModel` rejection and the
/// `aspect_ratio` / `image_size` / reference-count validation.
#[derive(Clone, Copy)]
pub struct ModelShape {
    /// The wire model id.
    pub id: &'static str,
    /// Whether the NB2-only knobs (`thinking_level`, `include_thoughts`,
    /// `use_grounding`, the extreme aspect ratios) are accepted.
    pub is_nb2: bool,
    /// Image sizes this model accepts.
    pub image_sizes: &'static [ImageSize],
    /// Max object-reference images.
    pub max_object_refs: usize,
    /// Max character-reference images.
    pub max_character_refs: usize,
}

/// The supported Nano Banana models (2026-05 survey). Order is the
/// `supported`-list order surfaced in `UnknownModel`.
pub const MODELS: &[ModelShape] = &[
    // NB1 — flash image. K1 only; no reference images; not NB2.
    ModelShape {
        id: "gemini-2.5-flash-image",
        is_nb2: false,
        image_sizes: &[ImageSize::K1],
        max_object_refs: 0,
        max_character_refs: 0,
    },
    // NB Pro — pro image preview. K1/K2/K4; 6 object / 5 character refs.
    ModelShape {
        id: "gemini-3-pro-image-preview",
        is_nb2: false,
        image_sizes: &[ImageSize::K1, ImageSize::K2, ImageSize::K4],
        max_object_refs: 6,
        max_character_refs: 5,
    },
    // NB2 — flash image preview (default). S512/K1/K2/K4; 10 object / 4
    // character refs; the NB2-only knobs + extreme aspect ratios.
    ModelShape {
        id: "gemini-3.1-flash-image-preview",
        is_nb2: true,
        image_sizes: &[ImageSize::S512, ImageSize::K1, ImageSize::K2, ImageSize::K4],
        max_object_refs: 10,
        max_character_refs: 4,
    },
];

/// Aspect ratios every model accepts.
const CROSS_MODEL_RATIOS: &[AspectRatio] = &[
    AspectRatio::ASPECT_RATIO_1_1,
    AspectRatio::ASPECT_RATIO_2_3,
    AspectRatio::ASPECT_RATIO_3_2,
    AspectRatio::ASPECT_RATIO_3_4,
    AspectRatio::ASPECT_RATIO_4_3,
    AspectRatio::ASPECT_RATIO_4_5,
    AspectRatio::ASPECT_RATIO_5_4,
    AspectRatio::ASPECT_RATIO_9_16,
    AspectRatio::ASPECT_RATIO_16_9,
    AspectRatio::ASPECT_RATIO_21_9,
];

/// Extreme aspect ratios only NB2 accepts.
const NB2_ONLY_RATIOS: &[AspectRatio] = &[
    AspectRatio::ASPECT_RATIO_1_4,
    AspectRatio::ASPECT_RATIO_1_8,
    AspectRatio::ASPECT_RATIO_4_1,
    AspectRatio::ASPECT_RATIO_8_1,
];

/// Look up a model's shape, or `None` if unsupported.
#[must_use]
pub fn lookup_model(model: &str) -> Option<&'static ModelShape> {
    MODELS.iter().find(|m| m.id == model)
}

/// Every supported model id, for the `UnknownModel { supported }` list.
#[must_use]
pub fn supported_model_ids() -> Vec<String> {
    MODELS.iter().map(|m| m.id.to_string()).collect()
}

/// The aspect ratios a model accepts.
fn supported_ratios(shape: &ModelShape) -> Vec<AspectRatio> {
    let mut out = CROSS_MODEL_RATIOS.to_vec();
    if shape.is_nb2 {
        out.extend_from_slice(NB2_ONLY_RATIOS);
    }
    out
}

/// The request facts a per-model validation pass needs. Bundled into a
/// struct so [`validate`] stays under the argument cap and the cap
/// builds it once from the wire kind.
pub struct ValidationInputs {
    pub aspect_ratio: AspectRatio,
    pub image_size: Option<ImageSize>,
    /// Whether the NB2-only knobs were set on the request.
    pub thinking_level_set: bool,
    pub include_thoughts_set: bool,
    pub use_grounding_set: bool,
    pub object_ref_count: usize,
    pub character_ref_count: usize,
}

/// Validate a Nano Banana request against its model shape *before* any
/// network dispatch. Returns the matching [`GeminiError`] on a miss.
///
/// `shape`'s model must already be resolved (an unknown model is
/// rejected by the cap before this is called). Checks, in order:
/// aspect ratio, image size, reference-path counts, and the NB2-only
/// knobs.
pub fn validate(shape: &ModelShape, inputs: &ValidationInputs) -> Result<(), GeminiError> {
    let ratios = supported_ratios(shape);
    if !ratios.contains(&inputs.aspect_ratio) {
        return Err(GeminiError::AspectRatioNotSupportedByModel {
            model: shape.id.to_string(),
            aspect_ratio: inputs.aspect_ratio,
            supported: ratios,
        });
    }

    if let Some(size) = inputs.image_size
        && !shape.image_sizes.contains(&size)
    {
        return Err(GeminiError::ImageSizeNotSupportedByModel {
            model: shape.id.to_string(),
            image_size: size,
            supported: shape.image_sizes.to_vec(),
        });
    }

    if inputs.object_ref_count > shape.max_object_refs {
        return Err(GeminiError::MissingRequiredField {
            model: shape.id.to_string(),
            field: format!(
                "object_reference_paths (max {}, got {})",
                shape.max_object_refs, inputs.object_ref_count
            ),
        });
    }
    if inputs.character_ref_count > shape.max_character_refs {
        return Err(GeminiError::MissingRequiredField {
            model: shape.id.to_string(),
            field: format!(
                "character_reference_paths (max {}, got {})",
                shape.max_character_refs, inputs.character_ref_count
            ),
        });
    }

    // The NB2-only knobs are rejected on older models. `MissingRequiredField`
    // is the closest taxonomy variant — the field is present but
    // unsupported here.
    if !shape.is_nb2 {
        for (set, field) in [
            (inputs.thinking_level_set, "thinking_level"),
            (inputs.include_thoughts_set, "include_thoughts"),
            (inputs.use_grounding_set, "use_grounding"),
        ] {
            if set {
                return Err(GeminiError::MissingRequiredField {
                    model: shape.id.to_string(),
                    field: format!("{field} (NB2-only)"),
                });
            }
        }
    }

    Ok(())
}

/// Parse the base64 image payload + grounding/thought metadata out of a
/// Nano Banana response. Factored out so a fixture-replay test locks the
/// shape.
pub fn parse_image_response(json: &str) -> Result<ParsedImage, String> {
    use serde_json::Value;

    let parsed: Value = serde_json::from_str(json).map_err(|e| format!("parse response: {e}"))?;

    let candidate = parsed
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| "response missing candidates[0]".to_string())?;

    // The image rides as an inline-data part inside the first
    // candidate's content parts.
    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(|| "response missing candidates[0].content.parts".to_string())?;

    let b64 = parts
        .iter()
        .filter_map(|p| p.get("inlineData").or_else(|| p.get("inline_data")))
        .find_map(|d| d.get("data").and_then(Value::as_str))
        .ok_or_else(|| "response has no inline image data part".to_string())?;

    let bytes = base64_decode(b64).map_err(|e| format!("decode image base64: {e}"))?;

    let thought_signature = parts
        .iter()
        .find_map(|p| p.get("thoughtSignature").and_then(Value::as_str))
        .map(ToString::to_string);

    let grounding = parse_grounding(candidate);

    Ok(ParsedImage {
        bytes,
        thought_signature,
        grounding,
    })
}

/// Pull the `groundingMetadata` block off a candidate, mapping
/// `webSearchQueries` to search queries and `groundingChunks[].uri` to
/// source URLs. Returns `None` when the block is absent. The grounding
/// is carried as inline `(search_queries, source_urls)` so the adapter
/// layer doesn't depend on the provider kinds in `aether-kinds`.
fn parse_grounding(candidate: &serde_json::Value) -> Option<(Vec<String>, Vec<String>)> {
    use serde_json::Value;

    let meta = candidate.get("groundingMetadata")?;

    let search_queries = meta
        .get("webSearchQueries")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    let source_urls = meta
        .get("groundingChunks")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|chunk| {
                    chunk
                        .get("web")
                        .and_then(|w| w.get("uri"))
                        .or_else(|| chunk.get("uri"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    Some((search_queries, source_urls))
}

/// Result of [`parse_image_response`].
pub struct ParsedImage {
    pub bytes: Vec<u8>,
    pub thought_signature: Option<String>,
    /// Grounding `(search_queries, source_urls)` parsed from
    /// `candidates[0].groundingMetadata`; `None` when absent.
    pub grounding: Option<(Vec<String>, Vec<String>)>,
}

/// Shared base64 decode for the media backends (the image path here and
/// the Lyria clip path in `lyria.rs`). Thin re-export of the local
/// decoder so both sit on one implementation.
pub fn decode_base64_for_media(input: &str) -> Result<Vec<u8>, String> {
    base64_decode(input)
}

/// Minimal standard-alphabet base64 decoder (no padding tolerance
/// beyond `=`). Avoids pulling a base64 crate into the dep graph for
/// the one decode the image path needs.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const fn val(c: u8) -> i16 {
        match c {
            b'A'..=b'Z' => (c - b'A') as i16,
            b'a'..=b'z' => (c - b'a' + 26) as i16,
            b'0'..=b'9' => (c - b'0' + 52) as i16,
            b'+' => 62,
            b'/' => 63,
            _ => -1,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c);
        if v < 0 {
            return Err(format!("invalid base64 char {:?}", c as char));
        }
        #[allow(clippy::cast_sign_loss)]
        let v = v as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            #[allow(clippy::cast_possible_truncation)]
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        ModelShape, ValidationInputs, base64_decode, lookup_model, parse_image_response,
        supported_model_ids, validate,
    };
    use crate::gemini::{AspectRatio, GeminiError, ImageSize};

    fn nb2() -> &'static ModelShape {
        lookup_model("gemini-3.1-flash-image-preview").expect("NB2 is a supported model")
    }
    fn nb1() -> &'static ModelShape {
        lookup_model("gemini-2.5-flash-image").expect("NB1 is a supported model")
    }

    /// Build a [`ValidationInputs`] with everything unset but the
    /// aspect ratio + image size.
    fn inputs(aspect_ratio: AspectRatio, image_size: Option<ImageSize>) -> ValidationInputs {
        ValidationInputs {
            aspect_ratio,
            image_size,
            thinking_level_set: false,
            include_thoughts_set: false,
            use_grounding_set: false,
            object_ref_count: 0,
            character_ref_count: 0,
        }
    }

    #[test]
    fn unknown_model_lookup_returns_none() {
        assert!(lookup_model("gemini-bogus").is_none());
        assert_eq!(supported_model_ids().len(), 3);
    }

    #[test]
    fn extreme_aspect_ratio_rejected_on_nb1() {
        let err = validate(nb1(), &inputs(AspectRatio::ASPECT_RATIO_8_1, None))
            .expect_err("ASPECT_RATIO_8_1 is NB2-only");
        let GeminiError::AspectRatioNotSupportedByModel { supported, .. } = err else {
            panic!("expected AspectRatioNotSupportedByModel, got {err:?}");
        };
        assert!(!supported.contains(&AspectRatio::ASPECT_RATIO_8_1));
    }

    #[test]
    fn extreme_aspect_ratio_accepted_on_nb2() {
        validate(
            nb2(),
            &inputs(AspectRatio::ASPECT_RATIO_8_1, Some(ImageSize::S512)),
        )
        .expect("ASPECT_RATIO_8_1 + S512 is valid on NB2");
    }

    #[test]
    fn image_size_rejected_when_unsupported_by_model() {
        let err = validate(
            nb1(),
            &inputs(AspectRatio::ASPECT_RATIO_1_1, Some(ImageSize::S512)),
        )
        .expect_err("S512 is NB2-only");
        assert!(matches!(
            err,
            GeminiError::ImageSizeNotSupportedByModel { .. }
        ));
    }

    #[test]
    fn nb2_accepts_high_res_sizes() {
        // NB2 (gemini-3.1-flash-image-preview) supports 512/1K/2K/4K — not
        // S512-only. K1/K2/K4 must all validate.
        for size in [ImageSize::K1, ImageSize::K2, ImageSize::K4] {
            validate(nb2(), &inputs(AspectRatio::ASPECT_RATIO_1_1, Some(size)))
                .unwrap_or_else(|e| panic!("NB2 should accept {size:?}: {e:?}"));
        }
    }

    #[test]
    fn over_count_object_refs_rejected() {
        let mut i = inputs(AspectRatio::ASPECT_RATIO_1_1, Some(ImageSize::K1));
        i.object_ref_count = 1;
        let err = validate(nb1(), &i).expect_err("NB1 accepts zero reference images");
        let GeminiError::MissingRequiredField { field, .. } = err else {
            panic!("expected MissingRequiredField, got {err:?}");
        };
        assert!(field.contains("object_reference_paths"));
    }

    #[test]
    fn nb2_only_knob_rejected_on_older_model() {
        let mut i = inputs(AspectRatio::ASPECT_RATIO_1_1, Some(ImageSize::K1));
        i.thinking_level_set = true;
        let err = validate(nb1(), &i).expect_err("thinking_level is NB2-only");
        let GeminiError::MissingRequiredField { field, .. } = err else {
            panic!("expected MissingRequiredField, got {err:?}");
        };
        assert!(field.contains("thinking_level"));
    }

    #[test]
    fn base64_decodes_known_vector() {
        // "Man" -> "TWFu"
        assert_eq!(base64_decode("TWFu").expect("decodes"), b"Man");
        // "" -> ""
        assert_eq!(base64_decode("").expect("decodes"), Vec::<u8>::new());
    }

    #[test]
    fn parses_fixture_response() {
        const FIXTURE: &str = include_str!("fixtures/nanobanana_v2_response.json");
        let parsed = parse_image_response(FIXTURE).expect("fixture is a valid NB response");
        // The fixture embeds the base64 of "Man" as a stand-in image.
        assert_eq!(parsed.bytes, b"Man");
        assert_eq!(parsed.thought_signature.as_deref(), Some("sig-abc"));
        // No `groundingMetadata` block in the v2 fixture.
        assert!(parsed.grounding.is_none());
    }

    #[test]
    fn parses_grounded_fixture_response() {
        const FIXTURE: &str = include_str!("fixtures/nanobanana_grounded_response.json");
        let parsed =
            parse_image_response(FIXTURE).expect("grounded fixture is a valid NB response");
        assert_eq!(parsed.bytes, b"Man");
        let (search_queries, source_urls) = parsed
            .grounding
            .expect("groundingMetadata block is present");
        assert_eq!(
            search_queries,
            vec![
                "current eiffel tower height".to_string(),
                "eiffel tower color 2026".to_string(),
            ]
        );
        assert_eq!(
            source_urls,
            vec![
                "https://en.wikipedia.org/wiki/Eiffel_Tower".to_string(),
                "https://www.toureiffel.paris/en".to_string(),
            ]
        );
    }

    #[test]
    fn missing_parts_errors() {
        assert!(parse_image_response(r#"{"candidates": []}"#).is_err());
    }
}
