use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};

/// Image bytes returned from the Gemini API. `mime_type` is what the API
/// reports (typically "image/png" for image-gen models).
pub struct ImageBytes {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// One reference image to feed alongside the text prompt.
pub struct Reference<'a> {
    pub bytes: &'a [u8],
    pub mime_type: &'a str,
}

/// Call the Gemini API to generate an image from a text prompt and an
/// optional list of reference images for subject/style conditioning.
///
/// Reads `GEMINI_API_KEY` from the environment. The model name is whatever
/// Google publishes for the image-gen surface (e.g. `gemini-3-pro-image`,
/// `gemini-2.5-flash-image`); pass it through verbatim and let the API
/// reply with a clear error if the name doesn't resolve.
pub fn generate_image(
    prompt: &str,
    model: &str,
    references: &[Reference<'_>],
) -> Result<ImageBytes> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .context("GEMINI_API_KEY not set in environment")?;

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
    );

    let mut parts: Vec<Value> = Vec::with_capacity(1 + references.len());
    parts.push(json!({"text": prompt}));
    for r in references {
        parts.push(json!({
            "inlineData": {
                "mimeType": r.mime_type,
                "data": B64.encode(r.bytes),
            }
        }));
    }

    let body = json!({
        "contents": [{"parts": parts}],
        "generationConfig": {
            "responseModalities": ["IMAGE"]
        }
    });

    let response = ureq::post(&url)
        .set("x-goog-api-key", &api_key)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(120))
        .send_json(body);

    let response = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_else(|_| "<unreadable>".into());
            bail!("gemini API returned status {code}: {body}");
        }
        Err(e) => return Err(anyhow!("gemini request failed: {e}")),
    };

    let value: Value = response.into_json().context("parsing gemini response JSON")?;

    extract_first_image(&value)
        .with_context(|| format!("extracting image from response: {value}"))
}

/// Call the Gemini API to generate text from a prompt + optional image
/// inputs. The image inputs share the `Reference` shape used by image
/// generation; here they're vision inputs for the text-out model.
pub fn generate_text(
    prompt: &str,
    references: &[Reference<'_>],
    model: &str,
) -> Result<String> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .context("GEMINI_API_KEY not set in environment")?;

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
    );

    let mut parts: Vec<Value> = Vec::with_capacity(1 + references.len());
    parts.push(json!({"text": prompt}));
    for r in references {
        parts.push(json!({
            "inlineData": {
                "mimeType": r.mime_type,
                "data": B64.encode(r.bytes),
            }
        }));
    }

    let body = json!({
        "contents": [{"parts": parts}],
    });

    let response = ureq::post(&url)
        .set("x-goog-api-key", &api_key)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(180))
        .send_json(body);

    let response = match response {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_else(|_| "<unreadable>".into());
            bail!("gemini text API returned status {code}: {body}");
        }
        Err(e) => return Err(anyhow!("gemini text request failed: {e}")),
    };

    let value: Value = response.into_json().context("parsing gemini response JSON")?;
    extract_first_text(&value)
        .with_context(|| format!("extracting text from response: {value}"))
}

fn extract_first_text(value: &Value) -> Result<String> {
    let candidates = value
        .get("candidates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no candidates array in response"))?;
    let first = candidates
        .first()
        .ok_or_else(|| anyhow!("candidates array is empty"))?;
    let parts = first
        .pointer("/content/parts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no content.parts in first candidate"))?;
    let mut out = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            out.push_str(text);
        }
    }
    if out.is_empty() {
        Err(anyhow!("no text part found in candidate"))
    } else {
        Ok(out)
    }
}

fn extract_first_image(value: &Value) -> Result<ImageBytes> {
    let candidates = value
        .get("candidates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no candidates array in response"))?;
    let first = candidates
        .first()
        .ok_or_else(|| anyhow!("candidates array is empty"))?;
    let parts = first
        .pointer("/content/parts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no content.parts in first candidate"))?;
    for part in parts {
        if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
            let data = inline
                .get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("inlineData missing data field"))?;
            let mime = inline
                .get("mimeType")
                .or_else(|| inline.get("mime_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let bytes = B64
                .decode(data)
                .context("decoding base64 image data")?;
            return Ok(ImageBytes {
                bytes,
                mime_type: mime,
            });
        }
    }
    Err(anyhow!("no inlineData part found in candidate"))
}
