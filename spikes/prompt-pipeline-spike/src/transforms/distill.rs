use anyhow::{Context, Result};

use crate::cache::Cache;
use crate::claude;
use crate::distills::Distill;
use crate::profiles::Profile;

const DEFAULT_MODEL: &str = "haiku";

/// Distill a Frame output to a target level of detail. Compresses while
/// preserving observer voice and key sensory anchors per the distill
/// template's directive.
///
/// Cache key includes the framed input bytes directly — same input +
/// same distill template + same model = same cached output.
pub fn distill(
    framed_input: &str,
    distill: &Distill,
    profile: Option<&Profile>,
    cache: &Cache,
) -> Result<String> {
    let prompt = distill.template.replace("{{INPUT}}", framed_input);

    // Profile override > distill template-declared model > DEFAULT_MODEL.
    let model = profile
        .and_then(|p| p.model_for("distill", &distill.frontmatter.id))
        .or(distill.frontmatter.model.as_deref())
        .unwrap_or(DEFAULT_MODEL);

    let key_parts = [
        framed_input,
        &distill.frontmatter.id,
        &distill.template,
        model,
    ];

    cache.get_or_compute(&key_parts, || {
        eprintln!(
            "distill: via {} ({}) — calling claude",
            distill.frontmatter.id, model
        );
        claude::complete(&prompt, model)
            .with_context(|| format!("distilling via {}", distill.frontmatter.id))
    })
}
