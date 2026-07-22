//! `ureq`-backed and disabled Gemini media backends for the native
//! `aether.gemini` cap (ADR-0050). The blocking calls run on the ADR-0093
//! spawn-and-die ephemeral worker; the pure request-body construction and the
//! wire-enum → provider-string maps live in [`body`](super::body), shared with
//! the ADR-0159 guest component.

use std::time::Duration;

use serde_json::Value;

use aether_contentgen::adapter::{
    AdapterUsage, GeminiAdapter, GeminiArtifact, GeminiImageRequest, GeminiMusicRequest, GeminiResponse,
};
use aether_contentgen::transport;

use super::body::{build_lyria_body, build_nanobanana_body, genlang_url};
use super::error;
use super::{lyria, nanobanana};

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
        Self { agent: transport::agent(), api_key, timeout }
    }
}

impl UreqGeminiAdapter {
    /// POST a JSON body to a Generative Language API endpoint and return
    /// the response text on a 2xx. Both media backends share this shape
    /// (build request → run → status-check), so it lives in one place.
    /// `endpoint` is the `:method` suffix (`generateContent` / `predict`).
    fn post_json(&self, model: &str, endpoint: &str, body: &Value) -> Result<String, String> {
        use ureq::http::Request;
        let body_bytes = serde_json::to_vec(body).map_err(|e| format!("encode request: {e}"))?;
        let http_req = Request::builder()
            .method("POST")
            .uri(genlang_url(model, endpoint))
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body_bytes)
            .map_err(|e| format!("build request: {e}"))?;
        let (status, retry_after_millis, text) = transport::run_request(&self.agent, http_req, self.timeout)?;
        if !(200..300).contains(&status) {
            return Err(format!("status={status} retry_after_millis={retry_after_millis:?} body={text}"));
        }
        Ok(text)
    }
}

impl GeminiAdapter for UreqGeminiAdapter {
    fn nanobanana_generate(&self, req: GeminiImageRequest) -> Result<GeminiResponse, String> {
        let body = build_nanobanana_body(&req);
        let text = self.post_json(&req.model, "generateContent", &body)?;

        let parsed = nanobanana::parse_image_response(&text)?;
        Ok(GeminiResponse {
            artifacts: vec![GeminiArtifact { bytes: parsed.bytes, ext: "png".to_string() }],
            model_used: req.model,
            usage: AdapterUsage::default(),
            thought_signature: parsed.thought_signature,
            grounding: parsed.grounding,
        })
    }

    fn lyria_generate(&self, req: GeminiMusicRequest) -> Result<GeminiResponse, String> {
        let body = build_lyria_body(&req.prompt, req.sample_count);
        let text = self.post_json(&req.model, "predict", &body)?;

        let clips = lyria::parse_clip_response(&text)?;
        let artifacts = clips.into_iter().map(|bytes| GeminiArtifact { bytes, ext: "wav".to_string() }).collect();
        Ok(GeminiResponse {
            artifacts,
            model_used: req.model,
            usage: AdapterUsage::default(),
            thought_signature: None,
            grounding: None,
        })
    }
}

#[cfg(test)]
mod tests {
    /// Real-API smoke for Lyria. Ignored by default.
    #[test]
    #[ignore = "needs GEMINI_API_KEY"]
    fn gemini_lyria_smoke() {
        use super::UreqGeminiAdapter;
        use aether_contentgen::adapter::{GeminiAdapter, GeminiMusicRequest};
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
