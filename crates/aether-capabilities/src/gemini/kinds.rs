//! Mail vocabulary for the `aether.gemini` media-generation cap
//! (ADR-0050 / ADR-0121). The cap is the sole receiver and agents /
//! guests addressing its mailbox are the only senders, so the kinds
//! live with their contract owner here rather than in `aether-kinds`.
//! The four `Kind`-deriving request/result types carry their
//! `inventory::submit!` descriptor registration via the derive; because
//! `aether-capabilities` links into every chassis binary, `describe_kinds`
//! and `descriptors::all()` keep surfacing them. `Usage` stays shared in
//! `aether-kinds` (the `aether.anthropic` kinds consume it too) and is
//! imported back here.

use serde::{Deserialize, Serialize};

use aether_kinds::Usage;

// ADR-0050 `aether.gemini` cap (issue 1015). Media generation only
// — image via Nano Banana, music via Lyria; no text completion (the
// user defaults to the Claude CLI per ADR-0050 §3). Two request
// kinds on the `aether.gemini` mailbox, each replying with a
// `*_result` Ok/Err enum carrying the shared `Usage` accounting on
// `Ok` and a provider-specific `GeminiError` on `Err`. Generated
// binary bytes never ride the wire: the reply carries a
// `save://gen/<uuid>.{png,wav}` path. The image schema is fixed by
// a 2026-05 API survey; per-model validation absorbs vendor drift.

/// Aspect ratio for a Nano Banana image. The cross-model set covers
/// `ASPECT_RATIO_1_1` … `ASPECT_RATIO_21_9`; the `ASPECT_RATIO_1_4` /
/// `ASPECT_RATIO_1_8` / `ASPECT_RATIO_4_1` / `ASPECT_RATIO_8_1`
/// extreme ratios are NB2-only and rejected on older models by the
/// adapter's per-model validation.
// Variant names mirror the provider's `W:H` aspect-ratio labels
// verbatim (`ASPECT_RATIO_16_9` = 16:9) so the wire vocabulary reads
// the same as the API survey; the `WxH`-camel form (`Ar16x9`) would
// obscure the mapping for the LLM caller building these.
#[allow(non_camel_case_types)]
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectRatio {
    ASPECT_RATIO_1_1,
    ASPECT_RATIO_2_3,
    ASPECT_RATIO_3_2,
    ASPECT_RATIO_3_4,
    ASPECT_RATIO_4_3,
    ASPECT_RATIO_4_5,
    ASPECT_RATIO_5_4,
    ASPECT_RATIO_9_16,
    ASPECT_RATIO_16_9,
    ASPECT_RATIO_21_9,
    ASPECT_RATIO_1_4,
    ASPECT_RATIO_1_8,
    ASPECT_RATIO_4_1,
    ASPECT_RATIO_8_1,
}

/// Output image size for a Nano Banana image. `S512` is NB2-only;
/// `K1` is supported by every model; `K2` / `K4` by NB Pro and NB2
/// (not the legacy NB1). The adapter enforces the per-model matrix.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    S512,
    K1,
    K2,
    K4,
}

/// Reasoning-effort knob for Nano Banana 2. `Minimal` / `High`;
/// rejected on older models by per-model validation.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Minimal,
    High,
}

/// Grounding metadata returned when `use_grounding=true` — the
/// search queries and source URLs the model consulted. Free-form
/// strings; the shape mirrors the provider's grounding payload
/// without locking the cap to a specific schema version.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GroundingMetadata {
    pub search_queries: Vec<String>,
    pub source_urls: Vec<String>,
}

/// Structured failure reason for a Gemini media generation
/// (ADR-0050 §1). `RateLimited` / `ContentPolicyRefused` /
/// `Unauthorized` mirror the Anthropic taxonomy; the
/// `*NotSupportedByModel` variants carry the rejected value plus the
/// model's supported set so the caller can correct and retry, and
/// `MissingRequiredField` names a per-model required field the
/// request omitted. `AdapterError` is the free-form catchall.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum GeminiError {
    RateLimited {
        retry_after_ms: Option<u32>,
    },
    ContentPolicyRefused,
    Unauthorized,
    UnknownModel {
        model: String,
        supported: Vec<String>,
    },
    AspectRatioNotSupportedByModel {
        model: String,
        aspect_ratio: AspectRatio,
        supported: Vec<AspectRatio>,
    },
    ImageSizeNotSupportedByModel {
        model: String,
        image_size: ImageSize,
        supported: Vec<ImageSize>,
    },
    MissingRequiredField {
        model: String,
        field: String,
    },
    AdapterError(String),
}

/// `aether.gemini.nanobanana.generate` — request an image from the
/// Nano Banana family. `model` selects `gemini-2.5-flash-image` /
/// `gemini-3-pro-image-preview` / `gemini-3.1-flash-image-preview`
/// (NB2, the default). Reference inputs arrive as file paths the cap
/// reads before dispatch. Per-model validation of `aspect_ratio` /
/// `image_size` / reference-path counts runs before any network
/// dispatch. Reply: `NanobananaGenerateResult` carrying a staged
/// `save://gen/<uuid>.png` path.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.gemini.nanobanana.generate")]
pub struct NanobananaGenerate {
    pub request_id: u64,
    pub model: String,
    pub prompt: String,
    pub aspect_ratio: AspectRatio,
    pub image_size: Option<ImageSize>,
    pub thinking_level: Option<ThinkingLevel>,
    pub include_thoughts: Option<bool>,
    pub object_reference_paths: Vec<String>,
    pub character_reference_paths: Vec<String>,
    pub use_grounding: Option<bool>,
    /// Opt-in / default-off. `None` / `Some(false)` clears the
    /// `thought_signature` from the reply (a signature can run to
    /// multiple MB and dominate the result); `Some(true)` retains it
    /// for a multi-turn continuation. Cross-model (Pro emits a
    /// signature too); gates only the reply populate, not validation.
    pub include_thought_signature: Option<bool>,
}

/// Reply to [`NanobananaGenerate`]. Both arms echo `request_id`.
/// `Ok` carries the staged image path (never inline bytes), the
/// model served, `Usage`, the NB2 `thought_signature` (passed back
/// unchanged for multi-turn), and grounding metadata when
/// `use_grounding=true`. `Err` carries a `GeminiError`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.gemini.nanobanana.generate_result")]
pub enum NanobananaGenerateResult {
    Ok {
        request_id: u64,
        output_path: String,
        model_used: String,
        usage: Usage,
        thought_signature: Option<String>,
        grounding: Option<GroundingMetadata>,
    },
    Err {
        request_id: u64,
        error: GeminiError,
    },
}

/// `aether.gemini.lyria.generate` — request music from the Lyria
/// family (snapshot 2026-05-20 of the Vertex AI Lyria API). `model`
/// selects `lyria-2` / `lyria-3` / `lyria-3-pro`. `seed` and
/// `sample_count` are mutually exclusive — the adapter rejects
/// both-set. Each clip is a fixed ~30s WAV at 48 kHz; there is no
/// `duration_s`. Reply: `LyriaGenerateResult` carrying one staged
/// `save://gen/<uuid>.wav` path per generated clip.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.gemini.lyria.generate")]
pub struct LyriaGenerate {
    pub request_id: u64,
    pub model: String,
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub seed: Option<u32>,
    pub sample_count: Option<u32>,
}

/// Reply to [`LyriaGenerate`]. Both arms echo `request_id`. `Ok`
/// carries one staged WAV path per clip (`sample_count` controls
/// the count, hence the plural `output_paths`), the model served,
/// and `Usage`. `Err` carries a `GeminiError`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.gemini.lyria.generate_result")]
pub enum LyriaGenerateResult {
    Ok {
        request_id: u64,
        output_paths: Vec<String>,
        model_used: String,
        usage: Usage,
    },
    Err {
        request_id: u64,
        error: GeminiError,
    },
}

#[cfg(test)]
mod tests {
    use super::{LyriaGenerate, LyriaGenerateResult, NanobananaGenerate, NanobananaGenerateResult};
    use aether_data::{Kind, SchemaType};
    use aether_kinds::descriptors;

    // The gemini kinds register through the `Kind` derive's
    // `inventory::submit!`; because `aether-capabilities` links into the
    // chassis, `aether_kinds::descriptors::all()` (which iterates the
    // global inventory slot) surfaces them. These guard that the move
    // out of `aether-kinds` kept them registered and structurally
    // unchanged (ADR-0050 / ADR-0121).

    #[test]
    fn gemini_kinds_are_registered_in_descriptor_inventory() {
        let descs = descriptors::all();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&NanobananaGenerate::NAME));
        assert!(names.contains(&NanobananaGenerateResult::NAME));
        assert!(names.contains(&LyriaGenerate::NAME));
        assert!(names.contains(&LyriaGenerateResult::NAME));
    }

    #[test]
    fn gemini_requests_are_structured_schemas() {
        // ADR-0050: both generate kinds carry String/Vec/Option fields.
        let descs = descriptors::all();
        for name in [NanobananaGenerate::NAME, LyriaGenerate::NAME] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: gemini request kind is registered in descriptor inventory");
            let SchemaType::Struct { repr_c, .. } = &d.schema else {
                panic!("{name} should be Struct, got {:?}", d.schema);
            };
            assert!(!*repr_c, "{name} contains String/Vec, must be structured");
        }
    }

    #[test]
    fn gemini_results_are_enum_schemas() {
        let descs = descriptors::all();
        for name in [NanobananaGenerateResult::NAME, LyriaGenerateResult::NAME] {
            let d = descs
                .iter()
                .find(|d| d.name == name)
                .expect("test setup: gemini result kind is registered in descriptor inventory");
            assert!(
                matches!(d.schema, SchemaType::Enum { .. }),
                "{name} should be Enum, got {:?}",
                d.schema
            );
        }
    }
}
