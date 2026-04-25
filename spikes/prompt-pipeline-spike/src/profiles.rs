use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub models: ModelAssignment,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelAssignment {
    /// Fallback model when no more-specific override matches.
    #[serde(default)]
    pub default: Option<String>,
    /// Per-transform-stage overrides keyed by stage name (`frame`, `distill`).
    /// Use to vary one stage's model independently of the other.
    #[serde(default)]
    pub by_stage: HashMap<String, String>,
    /// Per-lens-or-distill-id overrides. Most specific.
    #[serde(default)]
    pub by_lens: HashMap<String, String>,
}

impl Profile {
    /// Resolve the model to use for a given (stage, id). Priority:
    /// `by_lens` (most specific) → `by_stage` (per-transform-type) →
    /// `default` (fallback) → `None` (caller falls back to template's
    /// own model declaration).
    pub fn model_for(&self, stage: &str, id: &str) -> Option<&str> {
        self.models
            .by_lens
            .get(id)
            .map(|s| s.as_str())
            .or_else(|| self.models.by_stage.get(stage).map(|s| s.as_str()))
            .or_else(|| self.models.default.as_deref())
    }
}

pub fn load(path: &Path) -> Result<Profile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading profile {}", path.display()))?;
    serde_yaml::from_str(&content)
        .with_context(|| format!("parsing profile {}", path.display()))
}
