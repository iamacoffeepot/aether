use anyhow::{Context, Result};

use crate::facts::Fact;
use crate::gemini;
use crate::recipes::Recipe;

/// Default grading model. Vision-capable text-output Gemini.
const DEFAULT_GRADER_MODEL: &str = "gemini-3-pro-preview";

/// One reference image fed alongside the rendered image during grading.
/// The grader sees both so it can distinguish "carried by reference"
/// inferences from "out-of-scope inventions".
pub struct GraderReference {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub label: String,
}

/// Grade a rendered image against the recipe's fact corpus. Returns
/// the grader's full text response — typically a structured report
/// with three buckets: violations, conditional-dimension gaps,
/// out-of-scope inventions.
///
/// The grader sees:
/// - The rendered image
/// - Each reference image fed during generation, labeled
/// - The composed prompt that was dispatched
/// - Every fact (environmentals + per-entry) with frontmatter and body
/// - Recipe metadata (camera, aspect ratio)
pub fn grade(
    rendered_image: &[u8],
    rendered_image_mime: &str,
    references: &[GraderReference],
    recipe: &Recipe,
    composed_prompt: &str,
    facts: &[(String, Fact)],
    environmentals: &[(String, Fact)],
    model: Option<&str>,
) -> Result<String> {
    let model = model.unwrap_or(DEFAULT_GRADER_MODEL);
    let prompt = build_prompt(recipe, composed_prompt, facts, environmentals, references);

    let mut all_refs: Vec<gemini::Reference<'_>> =
        Vec::with_capacity(1 + references.len());
    all_refs.push(gemini::Reference {
        bytes: rendered_image,
        mime_type: rendered_image_mime,
    });
    for r in references {
        all_refs.push(gemini::Reference {
            bytes: &r.bytes,
            mime_type: &r.mime_type,
        });
    }

    eprintln!(
        "grade: model={model} prompt_len={} rendered={}b refs={} — calling gemini",
        prompt.len(),
        rendered_image.len(),
        references.len(),
    );
    gemini::generate_text(&prompt, &all_refs, model)
        .with_context(|| format!("grading via {model}"))
}

fn build_prompt(
    recipe: &Recipe,
    composed_prompt: &str,
    facts: &[(String, Fact)],
    environmentals: &[(String, Fact)],
    references: &[GraderReference],
) -> String {
    let mut s = String::new();

    s.push_str("# Fact-grounded image grading\n\n");
    s.push_str(
        "You are evaluating an image generated from a structured content-generation pipeline. \
         The first image attached is the rendered output. Subsequent images (if any) were fed as \
         reference inputs during generation — meaning the model was instructed to preserve their \
         identity / style. Your job is to compare the rendered image against the source corpus \
         (facts + recipe + composed prompt) and report what's right, what's wrong, and what's \
         underspecified.\n\n",
    );

    s.push_str("## Three buckets — categorize every observation as one of:\n\n");
    s.push_str(
        "1. **Violations** — declared facts the rendered image clearly fails to honor. Example: \
         a fact says \"matte finish\" and the image shows a glossy finish. Cite the fact id.\n\n",
    );
    s.push_str(
        "2. **Conditional-dimension gaps** — attribute dimensions that the facts establish as \
         live (e.g., chip-status applies once a material is declared chippable) but that the \
         recipe didn't pin. Note what dimension is open, which fact made it live, and what the \
         model picked. These aren't violations — they're corpus-completeness signals: \
         candidates for a new conditional fact.\n\n",
    );
    s.push_str(
        "3. **Out-of-scope inventions** — visible attributes that don't follow from any stated \
         or derivable fact AND aren't carried by a reference image. Example: a decorative \
         pattern on a fact that says \"no decorative relief\". Be skeptical here — if the \
         attribute could plausibly come from a reference image, it's NOT out-of-scope, it's \
         reference-carried.\n\n",
    );

    s.push_str("## Recipe metadata\n\n");
    s.push_str(&format!("- name: {}\n", recipe.recipe.name));
    if let Some(c) = &recipe.recipe.camera {
        s.push_str(&format!("- camera: {c}\n"));
    }
    if let Some(a) = &recipe.recipe.aspect_ratio {
        s.push_str(&format!("- aspect_ratio: {a}\n"));
    }
    s.push('\n');

    s.push_str("## Reference images fed during generation\n\n");
    if references.is_empty() {
        s.push_str("(none — this was a from-scratch render)\n\n");
    } else {
        for (idx, r) in references.iter().enumerate() {
            s.push_str(&format!(
                "- ref[{idx}]: {} ({} bytes, {})\n",
                r.label,
                r.bytes.len(),
                r.mime_type,
            ));
        }
        s.push_str(
            "\n(Attributes carried by these references — color, finish, style — should be \
             tagged 'reference-carried' in your reasoning, NOT out-of-scope inventions.)\n\n",
        );
    }

    s.push_str("## Environmentals\n\n");
    for (slot, fact) in environmentals {
        s.push_str(&format!("### {slot} = {}\n\n", fact.id()));
        s.push_str(&fact.body);
        s.push_str("\n\n");
    }

    s.push_str("## Per-entry facts (in recipe order)\n\n");
    for (id, fact) in facts {
        s.push_str(&format!("### {id} (type: {})\n\n", fact.fact_type()));
        s.push_str(&fact.body);
        s.push_str("\n\n");
    }

    s.push_str("## The composed prompt that was dispatched\n\n");
    s.push_str("```\n");
    s.push_str(composed_prompt);
    s.push_str("\n```\n\n");

    s.push_str("## Output format\n\n");
    s.push_str(
        "Respond in this exact markdown structure:\n\n\
         ## Violations\n\n\
         - list each as `fact-id: <one-line description of the failure>`. Empty list is fine.\n\n\
         ## Conditional-dimension gaps\n\n\
         - list each as `dimension (made live by fact-id): what the model picked`. Empty list \
         is fine.\n\n\
         ## Out-of-scope inventions\n\n\
         - list each as `attribute: why you flagged it`. Empty list is fine. Skip anything \
         attributable to a reference image.\n\n\
         ## Notes\n\n\
         - free-form: anything else worth flagging about fit, quality, or composition.\n",
    );

    s
}
