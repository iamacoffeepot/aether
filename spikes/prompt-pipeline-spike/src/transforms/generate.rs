use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::cache::BinaryCache;
use crate::gemini;

/// One reference image, owned form, with its mime type. Hash of the
/// bytes feeds into the Generate transform's cache key so swapping a
/// reference invalidates only the downstream image.
pub struct ReferenceImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Generate an image from a composed prompt + optional reference images
/// via Gemini. Returns the path to the cached PNG and the raw bytes.
///
/// Cache key is content-addressed over `(prompt, model, sha256(ref0),
/// sha256(ref1), …)` in declared order — same prompt + same model +
/// same references in same order = cache hit. Different prompt, model,
/// or references = different cache entry.
pub fn generate(
    composed_prompt: &str,
    model: &str,
    references: &[ReferenceImage],
    cache: &BinaryCache,
) -> Result<(PathBuf, Vec<u8>)> {
    let mut key_parts: Vec<String> = Vec::with_capacity(2 + references.len());
    key_parts.push(composed_prompt.to_string());
    key_parts.push(model.to_string());
    for r in references {
        let mut h = Sha256::new();
        h.update(&r.bytes);
        key_parts.push(format!("{:x}", h.finalize()));
    }
    let key_refs: Vec<&str> = key_parts.iter().map(|s| s.as_str()).collect();

    cache.get_or_compute(&key_refs, "png", || {
        eprintln!(
            "generate: model={model} prompt_len={} refs={} — calling gemini",
            composed_prompt.len(),
            references.len(),
        );
        let api_refs: Vec<gemini::Reference<'_>> = references
            .iter()
            .map(|r| gemini::Reference {
                bytes: &r.bytes,
                mime_type: &r.mime_type,
            })
            .collect();
        let img = gemini::generate_image(composed_prompt, model, &api_refs)
            .with_context(|| format!("generating image via {model}"))?;
        if !img.mime_type.starts_with("image/") {
            return Err(anyhow::anyhow!(
                "gemini returned non-image mime_type: {}",
                img.mime_type
            ));
        }
        Ok(img.bytes)
    })
}
