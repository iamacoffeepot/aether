//! `ureq`-backed and disabled Gemini media backends plus the request
//! encoding for the `aether.gemini` cap (ADR-0050). The blocking calls
//! run on the ADR-0093 spawn-and-die ephemeral worker; request bodies
//! and the wire-`AspectRatio` / `ImageSize` / `ThinkingLevel` → provider
//! `W:H` string mapping live here, alongside the adapter-error → typed
//! `GeminiError` mapping.

use std::time::Duration;

use serde_json::Value;

use crate::shared::contentgen::adapter::{
    AdapterUsage, GeminiAdapter, GeminiArtifact, GeminiImageRequest, GeminiMusicRequest,
    GeminiResponse,
};
use crate::shared::contentgen::shared;

use super::{AspectRatio, GeminiError, ImageSize, ThinkingLevel};
use super::{error, lyria, nanobanana};

/// Adapter returned when `GEMINI_API_KEY` is unset (or
/// `AETHER_GEMINI_DISABLE=1`). Every request replies
/// `Err { Unauthorized }` so a key-absent boot still loads rather than
/// warn-dropping.
pub struct DisabledGeminiAdapter;

impl GeminiAdapter for DisabledGeminiAdapter {
    fn nanobanana_generate(&self, _req: GeminiImageRequest) -> Result<GeminiResponse, String> {
        Err(error::UNAUTHORIZED_SENTINEL.to_string())
    }

    fn lyria_generate(&self, _req: GeminiMusicRequest) -> Result<GeminiResponse, String> {
        Err(error::UNAUTHORIZED_SENTINEL.to_string())
    }
}

/// `ureq`-backed Gemini media backend. Holds the shared agent, the API
/// key, and the per-request timeout. The blocking calls run on the
/// spawn-and-die ephemeral thread.
pub struct UreqGeminiAdapter {
    agent: ureq::Agent,
    api_key: String,
    timeout: Duration,
}

impl UreqGeminiAdapter {
    /// Build the adapter with a resolved key + timeout.
    #[must_use]
    pub fn new(api_key: String, timeout: Duration) -> Self {
        Self {
            agent: shared::agent(),
            api_key,
            timeout,
        }
    }
}

/// Generative Language API host.
const GENLANG_HOST: &str = "https://generativelanguage.googleapis.com";

impl UreqGeminiAdapter {
    /// POST a JSON body to a Generative Language API endpoint and return
    /// the response text on a 2xx. Both media backends share this shape
    /// (build request → run → status-check), so it lives in one place.
    /// `endpoint` is the `:method` suffix (`generateContent` / `predict`).
    fn post_json(&self, model: &str, endpoint: &str, body: &Value) -> Result<String, String> {
        use ureq::http::Request;
        let body_bytes = serde_json::to_vec(body).map_err(|e| format!("encode request: {e}"))?;
        let url = format!("{GENLANG_HOST}/v1beta/models/{model}:{endpoint}");
        let http_req = Request::builder()
            .method("POST")
            .uri(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body_bytes)
            .map_err(|e| format!("build request: {e}"))?;
        let (status, retry_after_millis, text) =
            shared::run_request(&self.agent, http_req, self.timeout)?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "status={status} retry_after_millis={retry_after_millis:?} body={text}"
            ));
        }
        Ok(text)
    }
}

/// Build the `generateContent` request body for a Nano Banana image
/// request: the prompt plus any reference images as inline-data parts,
/// and the per-request knobs in `generationConfig`. Each optional field
/// is emitted only when its source `Option` is `Some(..)` so an unset
/// knob leaves no key in the body. Factored out so a unit test can lock
/// the JSON shape without an HTTP call.
fn build_nanobanana_body(req: &GeminiImageRequest) -> Value {
    use serde_json::{Map, json};

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
    body.insert(
        "contents".to_string(),
        json!([{ "role": "user", "parts": parts }]),
    );
    body.insert(
        "generationConfig".to_string(),
        Value::Object(generation_config),
    );
    if req.use_grounding {
        body.insert("tools".to_string(), json!([{ "google_search": {} }]));
    }

    Value::Object(body)
}

impl GeminiAdapter for UreqGeminiAdapter {
    fn nanobanana_generate(&self, req: GeminiImageRequest) -> Result<GeminiResponse, String> {
        let body = build_nanobanana_body(&req);
        let text = self.post_json(&req.model, "generateContent", &body)?;

        let parsed = nanobanana::parse_image_response(&text)?;
        Ok(GeminiResponse {
            artifacts: vec![GeminiArtifact {
                bytes: parsed.bytes,
                ext: "png".to_string(),
            }],
            model_used: req.model,
            usage: AdapterUsage::default(),
            thought_signature: parsed.thought_signature,
            grounding: parsed.grounding,
        })
    }

    fn lyria_generate(&self, req: GeminiMusicRequest) -> Result<GeminiResponse, String> {
        use serde_json::json;

        let body = json!({
            "instances": [{ "prompt": req.prompt }],
            "parameters": { "sampleCount": req.sample_count.max(1) },
        });
        let text = self.post_json(&req.model, "predict", &body)?;

        let clips = lyria::parse_clip_response(&text)?;
        let artifacts = clips
            .into_iter()
            .map(|bytes| GeminiArtifact {
                bytes,
                ext: "wav".to_string(),
            })
            .collect();
        Ok(GeminiResponse {
            artifacts,
            model_used: req.model,
            usage: AdapterUsage::default(),
            thought_signature: None,
            grounding: None,
        })
    }
}

/// Minimal standard-alphabet base64 encoder for reference-image bytes
/// on the request side (no padding omitted). Avoids a base64 crate.
fn base64_encode(bytes: &[u8]) -> String {
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
pub fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    use ThinkingLevel as T;
    match level {
        T::Minimal => "minimal",
        T::High => "high",
    }
}

/// Convert an adapter error string into the typed `GeminiError`.
pub fn map_adapter_error(raw: &str) -> GeminiError {
    error::adapter_error_to_typed(raw)
}

#[cfg(test)]
mod tests {
    /// With every knob set, the request body carries
    /// `imageConfig.imageSize`, `thinkingConfig.thinkingLevel` /
    /// `includeThoughts`, and `tools[0].google_search` (issue 1167).
    #[test]
    fn nanobanana_body_carries_set_params() {
        use crate::shared::contentgen::adapter::GeminiImageRequest;
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
        use crate::shared::contentgen::adapter::GeminiImageRequest;
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

    /// Real-API smoke for Lyria. Ignored by default.
    #[test]
    #[ignore = "needs GEMINI_API_KEY"]
    fn gemini_lyria_smoke() {
        use super::UreqGeminiAdapter;
        use crate::shared::contentgen::adapter::{GeminiAdapter, GeminiMusicRequest};
        use std::env;
        use std::time::Duration;
        // Test-only: the live-API smoke reads an external credential
        // (GEMINI_API_KEY), not cap config; gated `#[ignore]`.
        #[allow(clippy::disallowed_methods)]
        let key = env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY set for smoke");
        let adapter = UreqGeminiAdapter::new(key, Duration::from_mins(2));
        let resp = adapter
            .lyria_generate(GeminiMusicRequest {
                model: "lyria-3".to_string(),
                prompt: "calm ambient pad".to_string(),
                sample_count: 1,
            })
            .expect("live lyria request succeeds");
        assert!(!resp.artifacts.is_empty());
    }
}
