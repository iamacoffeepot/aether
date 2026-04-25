use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::frontmatter;

#[derive(Debug, Clone, Deserialize)]
pub struct LensFrontmatter {
    pub id: String,
    pub applies_to: String,
    #[serde(default)]
    pub requires_observer: bool,
    #[serde(default)]
    pub slots: Vec<SlotDescriptor>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SlotDescriptor {
    pub name: String,
    pub fact_type: String,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct Lens {
    pub frontmatter: LensFrontmatter,
    pub template: String,
}

pub fn load(path: &Path) -> Result<Lens> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading lens {}", path.display()))?;
    let (fm_text, template) = frontmatter::split(&content)
        .with_context(|| format!("splitting frontmatter for {}", path.display()))?;
    let frontmatter: LensFrontmatter =
        serde_yaml::from_str(&fm_text).context("parsing lens frontmatter")?;
    Ok(Lens {
        frontmatter,
        template: template.trim().to_string(),
    })
}

pub fn load_by_id(lenses_root: &Path, id: &str) -> Result<Lens> {
    // id like "material.feeling" → lenses/material/feeling.md
    let parts: Vec<&str> = id.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "lens id must be of form '<fact_type>.<name>': got {id}"
        ));
    }
    let path = lenses_root.join(parts[0]).join(format!("{}.md", parts[1]));
    load(&path)
}
