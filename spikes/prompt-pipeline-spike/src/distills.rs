use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::frontmatter;

#[derive(Debug, Clone, Deserialize)]
pub struct DistillFrontmatter {
    pub id: String,
    pub applies_to: String, // fact_type this distill operates on
    pub target_lod: String,
    #[serde(default)]
    pub target_length: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Distill {
    pub frontmatter: DistillFrontmatter,
    pub template: String,
}

pub fn load(path: &Path) -> Result<Distill> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading distill template {}", path.display()))?;
    let (fm_text, template) = frontmatter::split(&content)
        .with_context(|| format!("splitting frontmatter for {}", path.display()))?;
    let frontmatter: DistillFrontmatter =
        serde_yaml::from_str(&fm_text).context("parsing distill frontmatter")?;
    Ok(Distill {
        frontmatter,
        template: template.trim().to_string(),
    })
}

pub fn load_for(distills_root: &Path, fact_type: &str, lod: &str) -> Result<Distill> {
    // distill/<fact_type>/<lod>.md
    let path = distills_root.join(fact_type).join(format!("{lod}.md"));
    load(&path)
}
