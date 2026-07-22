//! Pure request-side logic for the `aether.gemini` provider (ADR-0050,
//! ADR-0159 §2): the Generative Language API endpoint URL, the Nano Banana
//! `generateContent` / Lyria `predict` request-body builders, the
//! request-side base64 encoder for reference images, the wire-enum → provider
//! `W:H` / size / thinking-level string maps, and the adapter-error → typed
//! `GeminiError` mapping.
//!
//! No I/O and no `ureq` — these compile to `wasm32` unchanged, so the guest
//! component builds the same request bodies the native cap does. The blocking
//! HTTP call that consumes these bodies lives in the runtime-gated
//! [`adapter`](super::adapter) (native) or rides `aether.http.fetch` (guest).

use serde_json::{Map, Value, json};

use aether_contentgen::adapter::GeminiImageRequest;

use super::{AspectRatio, ImageSize, ThinkingLevel};
// `map_adapter_error` (and its `GeminiError` return + the `error` module) are
// native-only: only the `ureq` backend produces the `status=<n>` sentinel it
// parses.
#[cfg(feature = "runtime")]
use super::GeminiError;
#[cfg(feature = "runtime")]
use super::error;

/// Generative Language API host.
pub const GENLANG_HOST: &str = "https://generativelanguage.googleapis.com";

/// The Generative Language API URL for `model`'s `endpoint` method
/// (`generateContent` for images, `predict` for music). Both halves build the
/// same URL through this one place.
#[must_use]
pub fn genlang_url(model: &str, endpoint: &str) -> String {
    format!("{GENLANG_HOST}/v1beta/models/{model}:{endpoint}")
}

/// Build the `generateContent` request body for a Nano Banana image
/// request: the prompt plus any reference images as inline-data parts,
/// and the per-request knobs in `generationConfig`. Each optional field
/// is emitted only when its source `Option` is `Some(..)` so an unset
/// knob leaves no key in the body. Factored out so a unit test can lock
/// the JSON shape without an HTTP call.
#[must_use]
pub fn build_nanobanana_body(req: &GeminiImageRequest) -> Value {
    let mut parts = vec![json!({ "text": req.prompt })];
    for img in &req.reference_images {
        parts.push(json!({
            "inlineData": {
                "mimeType": "image/png",
                "data": base64_encode(img),
            }
        }));
    }

    // `imageConfig` always carries the aspect ratio; `imageSize` rides
    // alongside it under the same object when set (issue 1167 — do not
    // switch to `responseFormat.image`).
    let mut image_config = Map::new();
    image_config.insert("aspectRatio".to_string(), json!(req.aspect_ratio));
    if let Some(size) = &req.image_size {
        image_config.insert("imageSize".to_string(), json!(size));
    }

    let mut generation_config = Map::new();
    generation_config.insert("imageConfig".to_string(), Value::Object(image_config));

    // `thinkingConfig` only appears when at least one of its fields is
    // set; each field is emitted independently.
    let mut thinking_config = Map::new();
    if let Some(level) = &req.thinking_level {
        thinking_config.insert("thinkingLevel".to_string(), json!(level));
    }
    if let Some(include) = req.include_thoughts {
        thinking_config.insert("includeThoughts".to_string(), json!(include));
    }
    if !thinking_config.is_empty() {
        generation_config.insert("thinkingConfig".to_string(), Value::Object(thinking_config));
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), json!([{ "role": "user", "parts": parts }]));
    body.insert("generationConfig".to_string(), Value::Object(generation_config));
    if req.use_grounding {
        body.insert("tools".to_string(), json!([{ "google_search": {} }]));
    }

    Value::Object(body)
}

/// Build the Vertex Lyria `predict` request body: one instance carrying the
/// prompt, and `sampleCount` (clamped to at least 1) in `parameters`.
#[must_use]
pub fn build_lyria_body(prompt: &str, sample_count: u32) -> Value {
    json!({
        "instances": [{ "prompt": prompt }],
        "parameters": { "sampleCount": sample_count.max(1) },
    })
}

/// Minimal standard-alphabet base64 encoder for reference-image bytes
/// on the request side (no padding omitted). Avoids a base64 crate.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Map the wire `AspectRatio` to the provider's `W:H` string.
#[must_use]
pub fn aspect_ratio_str(ar: AspectRatio) -> &'static str {
    use AspectRatio as A;
    match ar {
        A::ASPECT_RATIO_1_1 => "1:1",
        A::ASPECT_RATIO_2_3 => "2:3",
        A::ASPECT_RATIO_3_2 => "3:2",
        A::ASPECT_RATIO_3_4 => "3:4",
        A::ASPECT_RATIO_4_3 => "4:3",
        A::ASPECT_RATIO_4_5 => "4:5",
        A::ASPECT_RATIO_5_4 => "5:4",
        A::ASPECT_RATIO_9_16 => "9:16",
        A::ASPECT_RATIO_16_9 => "16:9",
        A::ASPECT_RATIO_21_9 => "21:9",
        A::ASPECT_RATIO_1_4 => "1:4",
        A::ASPECT_RATIO_1_8 => "1:8",
        A::ASPECT_RATIO_4_1 => "4:1",
        A::ASPECT_RATIO_8_1 => "8:1",
    }
}

/// Map the wire `ImageSize` to the provider's `imageConfig.imageSize`
/// string. Uppercase `K`; `"512"` has no `K`.
#[must_use]
pub fn image_size_str(size: ImageSize) -> &'static str {
    use ImageSize as S;
    match size {
        S::S512 => "512",
        S::K1 => "1K",
        S::K2 => "2K",
        S::K4 => "4K",
    }
}

/// Map the wire `ThinkingLevel` to the provider's
/// `thinkingConfig.thinkingLevel` string.
#[must_use]
pub fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    use ThinkingLevel as T;
    match level {
        T::Minimal => "minimal",
        T::High => "high",
    }
}

/// Convert an adapter error string into the typed `GeminiError`. Only the
/// native `ureq` backend prepends the `status=<n>` sentinel this parses; the
/// guest reads the status off `FetchResult` directly, so this is runtime-only.
#[cfg(feature = "runtime")]
#[must_use]
pub fn map_adapter_error(raw: &str) -> GeminiError {
    error::adapter_error_to_typed(raw)
}

#[cfg(test)]
mod tests {
    use aether_contentgen::adapter::GeminiImageRequest;

    /// With every knob set, the request body carries
    /// `imageConfig.imageSize`, `thinkingConfig.thinkingLevel` /
    /// `includeThoughts`, and `tools[0].google_search` (issue 1167).
    #[test]
    fn nanobanana_body_carries_set_params() {
        let body = super::build_nanobanana_body(&GeminiImageRequest {
            model: "gemini-3.1-flash-image-preview".to_string(),
            prompt: "a cat".to_string(),
            aspect_ratio: "16:9".to_string(),
            image_size: Some("2K".to_string()),
            thinking_level: Some("high".to_string()),
            include_thoughts: Some(true),
            use_grounding: true,
            reference_images: Vec::new(),
        });
        let gcfg = &body["generationConfig"];
        assert_eq!(gcfg["imageConfig"]["aspectRatio"], "16:9");
        assert_eq!(gcfg["imageConfig"]["imageSize"], "2K");
        assert_eq!(gcfg["thinkingConfig"]["thinkingLevel"], "high");
        assert_eq!(gcfg["thinkingConfig"]["includeThoughts"], true);
        assert_eq!(body["tools"][0]["google_search"], serde_json::json!({}));
    }

    /// With the optional knobs unset, the body has no `imageSize`,
    /// no `thinkingConfig`, and no `tools` key — only the always-on
    /// `aspectRatio` survives under `imageConfig`.
    #[test]
    fn nanobanana_body_omits_unset_params() {
        let body = super::build_nanobanana_body(&GeminiImageRequest {
            model: "gemini-3.1-flash-image-preview".to_string(),
            prompt: "a cat".to_string(),
            aspect_ratio: "1:1".to_string(),
            reference_images: Vec::new(),
            ..Default::default()
        });
        let gcfg = &body["generationConfig"];
        assert_eq!(gcfg["imageConfig"]["aspectRatio"], "1:1");
        assert!(gcfg["imageConfig"].get("imageSize").is_none());
        assert!(gcfg.get("thinkingConfig").is_none());
        assert!(body.get("tools").is_none());
    }

    /// The Lyria body carries the prompt as an instance and clamps
    /// `sampleCount` to at least 1.
    #[test]
    fn lyria_body_clamps_sample_count() {
        let body = super::build_lyria_body("ambient pad", 0);
        assert_eq!(body["instances"][0]["prompt"], "ambient pad");
        assert_eq!(body["parameters"]["sampleCount"], 1);
        let two = super::build_lyria_body("ambient pad", 2);
        assert_eq!(two["parameters"]["sampleCount"], 2);
    }
}
