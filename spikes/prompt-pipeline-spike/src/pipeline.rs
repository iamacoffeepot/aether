use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cache::{BinaryCache, Cache};
use crate::compose;
use crate::distills;
use crate::facts;
use crate::lenses;
use crate::observers;
use crate::profiles::Profile;
use crate::recipes;
use crate::transforms::{distill, frame, generate};

pub struct PipelineResult {
    pub composed_prompt: String,
    pub blocks: Vec<(String, String)>, // (fact_id, framed_block) in recipe order
    pub image: Option<GeneratedImage>,
    pub recipe: recipes::Recipe,
    pub facts: Vec<(String, facts::Fact)>,           // (entry_id, loaded fact) in recipe order
    pub environmentals: Vec<(String, facts::Fact)>,  // (slot, loaded fact)
}

pub struct GeneratedImage {
    pub cache_path: PathBuf,
    pub model: String,
    pub byte_len: usize,
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub copied_to: Option<PathBuf>,
    pub references: Vec<LoadedReference>,
}

pub struct LoadedReference {
    pub path: String,
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub label: Option<String>,
}

fn guess_mime_type(path: &str) -> String {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") { "image/png".into() }
    else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") { "image/jpeg".into() }
    else if lower.ends_with(".webp") { "image/webp".into() }
    else { "image/png".into() }
}

pub fn run(
    root: &Path,
    recipe_path: &Path,
    profile: Option<&Profile>,
    do_generate: bool,
) -> Result<PipelineResult> {
    let recipe = recipes::load(recipe_path)?;

    let facts_root = root.join("facts");
    let lenses_root = root.join("lenses");
    let observers_root = root.join("observers");
    let distills_root = root.join("distill");
    let cache = Cache::new(root.join("cache").join("blocks"));
    let image_cache = BinaryCache::new(root.join("cache").join("images"));

    // Load environmental facts. Keys are slot names (e.g. "lighting"), values
    // are the loaded Fact for the declared id (e.g. "lighting.window-morning").
    let env_facts: HashMap<String, facts::Fact> = recipe
        .environmentals
        .iter()
        .map(|(slot, fact_id)| -> Result<_> {
            let f = facts::load_by_id(&facts_root, fact_id)
                .with_context(|| format!("loading environmental {slot}={fact_id}"))?;
            Ok((slot.clone(), f))
        })
        .collect::<Result<_>>()?;

    // Load observer if specified.
    let observer = match &recipe.observer {
        Some(r) => Some(
            observers::load_by_id(&observers_root, &r.id)
                .with_context(|| format!("loading observer {}", r.id))?,
        ),
        None => None,
    };

    // Process each fact entry.
    let mut entries: Vec<(u32, String, String)> = Vec::new();
    for entry in &recipe.facts {
        let fact = facts::load_by_id(&facts_root, &entry.fact)
            .with_context(|| format!("loading fact {}", entry.fact))?;
        let lens = lenses::load_by_id(&lenses_root, &entry.lens)
            .with_context(|| format!("loading lens {}", entry.lens))?;

        if fact.fact_type() != lens.frontmatter.applies_to {
            return Err(anyhow!(
                "fact {} (type '{}') does not match lens {} (applies_to '{}')",
                fact.id(),
                fact.fact_type(),
                lens.frontmatter.id,
                lens.frontmatter.applies_to
            ));
        }

        // Slot fills for this lens — only the slots the lens declares.
        let slot_facts: HashMap<String, &facts::Fact> = lens
            .frontmatter
            .slots
            .iter()
            .filter_map(|slot| {
                env_facts
                    .get(&slot.name)
                    .map(|f| (slot.name.clone(), f))
            })
            .collect();

        let observer_ref = if lens.frontmatter.requires_observer {
            observer.as_ref()
        } else {
            None
        };

        let framed = frame::frame(&fact, &lens, &slot_facts, observer_ref, profile, &cache)
            .with_context(|| format!("framing {} via {}", fact.id(), lens.frontmatter.id))?;

        // Optional Distill stage: if the recipe entry specifies an `lod`,
        // load the per-(fact_type, lod) distill template and compress.
        let final_block = if let Some(lod) = &entry.lod {
            let distill_template = distills::load_for(&distills_root, fact.fact_type(), lod)
                .with_context(|| {
                    format!(
                        "loading distill template for fact_type={} lod={}",
                        fact.fact_type(),
                        lod
                    )
                })?;
            distill::distill(&framed, &distill_template, profile, &cache).with_context(|| {
                format!(
                    "distilling {} via {} at lod={}",
                    fact.id(),
                    distill_template.frontmatter.id,
                    lod
                )
            })?
        } else {
            framed
        };

        entries.push((entry.order, fact.id().to_string(), final_block));
    }

    entries.sort_by_key(|(order, _, _)| *order);
    let display_blocks: Vec<(String, String)> = entries
        .iter()
        .map(|(_, id, block)| (id.clone(), block.clone()))
        .collect();

    let composed = compose::compose(
        entries.into_iter().map(|(o, _, b)| (o, b)).collect(),
        recipe.recipe.camera.as_deref(),
        recipe.recipe.aspect_ratio.as_deref(),
    );

    let image = if do_generate {
        let cfg = recipe
            .generate
            .as_ref()
            .ok_or_else(|| anyhow!("--generate requested but recipe has no [generate] section"))?;
        let mut loaded_refs: Vec<LoadedReference> = Vec::with_capacity(cfg.references.len());
        let mut transform_refs: Vec<generate::ReferenceImage> = Vec::with_capacity(cfg.references.len());
        for entry in &cfg.references {
            let path = root.join(&entry.path);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading reference image {}", path.display()))?;
            let mime_type = guess_mime_type(&entry.path);
            eprintln!(
                "  reference: {} ({} bytes, {}{})",
                entry.path,
                bytes.len(),
                mime_type,
                entry.label.as_deref().map(|l| format!(" — {l}")).unwrap_or_default(),
            );
            loaded_refs.push(LoadedReference {
                path: entry.path.clone(),
                bytes: bytes.clone(),
                mime_type: mime_type.clone(),
                label: entry.label.clone(),
            });
            transform_refs.push(generate::ReferenceImage { bytes, mime_type });
        }
        let (cache_path, bytes) = generate::generate(&composed, &cfg.model, &transform_refs, &image_cache)?;
        let mime_type = guess_mime_type_from_bytes(&bytes);
        let copied_to = if let Some(rel) = &cfg.output_path {
            let dest = root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating output dir {}", parent.display()))?;
            }
            std::fs::copy(&cache_path, &dest)
                .with_context(|| format!("copying image to {}", dest.display()))?;
            Some(dest)
        } else {
            None
        };
        Some(GeneratedImage {
            cache_path,
            model: cfg.model.clone(),
            byte_len: bytes.len(),
            bytes,
            mime_type,
            copied_to,
            references: loaded_refs,
        })
    } else {
        None
    };

    let display_facts: Vec<(String, facts::Fact)> = recipe
        .facts
        .iter()
        .map(|entry| -> Result<_> {
            let f = facts::load_by_id(&facts_root, &entry.fact)?;
            Ok((entry.fact.clone(), f))
        })
        .collect::<Result<_>>()?;
    let display_envs: Vec<(String, facts::Fact)> = env_facts
        .into_iter()
        .collect();

    Ok(PipelineResult {
        composed_prompt: composed,
        blocks: display_blocks,
        image,
        recipe,
        facts: display_facts,
        environmentals: display_envs,
    })
}

fn guess_mime_type_from_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { "image/png".into() }
    else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) { "image/jpeg".into() }
    else { "image/png".into() } // best-effort default
}
