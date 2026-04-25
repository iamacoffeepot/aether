use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub recipe: RecipeMeta,
    #[serde(default)]
    pub environmentals: HashMap<String, String>,
    pub observer: Option<ObserverRef>,
    pub facts: Vec<FactEntry>,
    #[serde(default)]
    pub generate: Option<GenerateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GenerateConfig {
    pub model: String,
    /// Optional human-readable destination for the generated image,
    /// relative to the spike root. The cache always retains the
    /// content-addressed copy; this is just a convenient symlink/copy
    /// destination for inspection.
    #[serde(default)]
    pub output_path: Option<String>,
    /// Reference images to feed alongside the text prompt (subject /
    /// style conditioning). Paths are relative to the spike root.
    /// Order is load-bearing — Gemini weights references in declared
    /// order alongside the text prompt.
    #[serde(default)]
    pub references: Vec<ReferenceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReferenceEntry {
    pub path: String,
    /// Optional descriptive label, surfaced in logs only. Not part of
    /// the prompt or the cache key.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecipeMeta {
    pub name: String,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub camera: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObserverRef {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FactEntry {
    pub fact: String,
    pub lens: String,
    pub order: u32,
    /// Optional level-of-detail label. If set, the Frame output is
    /// distilled through `distill/<fact_type>/<lod>.md` before
    /// composition. If unset, the Frame output passes straight to
    /// the composer.
    #[serde(default)]
    pub lod: Option<String>,
}

pub fn load(path: &Path) -> Result<Recipe> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading recipe {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parsing recipe {}", path.display()))
}
