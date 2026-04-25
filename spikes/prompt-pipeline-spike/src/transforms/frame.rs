use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;

use crate::cache::Cache;
use crate::claude;
use crate::facts::Fact;
use crate::lenses::Lens;
use crate::observers::Observer;
use crate::profiles::Profile;

const DEFAULT_MODEL: &str = "haiku";

/// Frame a fact through a lens, filling slots with environmental fact bodies
/// (AsFact verbatim — no recursive framing) and optionally an observer.
///
/// For declarative lenses (`requires_observer: false`), this is a single
/// claude call with the template's `{{FACT}}` and slot placeholders filled.
///
/// For perception lenses (`requires_observer: true`), this is the simplified
/// single-call form for the spike: fact + slot fills + observer all into one
/// template, one claude call. The full architecture decomposes this into
/// parallel `Impose<fact, modifier>` calls + a synthesizing call; deferred to
/// the next spike iteration once observer-differentiation is validated here.
pub fn frame(
    fact: &Fact,
    lens: &Lens,
    environmentals: &HashMap<String, &Fact>,
    observer: Option<&Observer>,
    profile: Option<&Profile>,
    cache: &Cache,
) -> Result<String> {
    // Validate slot fills.
    for slot in &lens.frontmatter.slots {
        let provided = environmentals.get(&slot.name);
        if slot.required && provided.is_none() {
            return Err(anyhow!(
                "lens {} requires slot '{}' but recipe didn't provide it",
                lens.frontmatter.id,
                slot.name
            ));
        }
        if let Some(fact) = provided {
            if fact.fact_type() != slot.fact_type {
                return Err(anyhow!(
                    "lens {} slot '{}' expects fact_type '{}' but got '{}' (fact: {})",
                    lens.frontmatter.id,
                    slot.name,
                    slot.fact_type,
                    fact.fact_type(),
                    fact.id()
                ));
            }
        }
    }

    // Validate observer requirement.
    if lens.frontmatter.requires_observer && observer.is_none() {
        return Err(anyhow!(
            "lens {} requires observer but recipe didn't provide one",
            lens.frontmatter.id
        ));
    }

    // Fill template.
    let mut prompt = lens.template.clone();
    prompt = prompt.replace("{{FACT}}", &fact.body);

    // Iterate the lens's declared slots so optional unfilled slots get
    // replaced with empty strings rather than left as raw `{{NAME}}`
    // placeholders in the prompt.
    for slot in &lens.frontmatter.slots {
        let placeholder = format!("{{{{{}}}}}", slot.name.to_uppercase());
        let value = environmentals
            .get(&slot.name)
            .map(|f| f.body.as_str())
            .unwrap_or("");
        prompt = prompt.replace(&placeholder, value);
    }

    if let Some(obs) = observer {
        prompt = prompt.replace("{{OBSERVER}}", &obs.body);
    }

    // Profile override > lens-declared model > DEFAULT_MODEL.
    let model = profile
        .and_then(|p| p.model_for("frame", &lens.frontmatter.id))
        .or(lens.frontmatter.model.as_deref())
        .unwrap_or(DEFAULT_MODEL);

    // Build cache key from all inputs that affect output.
    let env_key = {
        let mut entries: Vec<(&String, &&Fact)> = environmentals.iter().collect();
        entries.sort_by_key(|(k, _)| k.as_str());
        entries
            .into_iter()
            .map(|(k, v)| format!("{k}={}::{}", v.id(), v.body))
            .collect::<Vec<_>>()
            .join("|")
    };
    let observer_key = observer
        .map(|o| format!("{}::{}", o.frontmatter.id, o.body))
        .unwrap_or_default();
    let key_parts = [
        fact.id(),
        &fact.body,
        &lens.frontmatter.id,
        &lens.template,
        &env_key,
        &observer_key,
        model,
    ];

    cache.get_or_compute(&key_parts, || {
        eprintln!(
            "frame: {} via {} ({}) — calling claude",
            fact.id(),
            lens.frontmatter.id,
            model
        );
        claude::complete(&prompt, model).with_context(|| {
            format!(
                "framing {} via {}",
                fact.id(),
                lens.frontmatter.id
            )
        })
    })
}
