use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::frontmatter;

#[derive(Debug, Clone, Deserialize)]
pub struct FactFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub fact_type: String,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct Fact {
    pub frontmatter: FactFrontmatter,
    pub body: String,
}

impl Fact {
    pub fn fact_type(&self) -> &str {
        &self.frontmatter.fact_type
    }

    pub fn id(&self) -> &str {
        &self.frontmatter.id
    }
}

pub fn load(path: &Path) -> Result<Fact> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading fact {}", path.display()))?;
    let (fm_text, body) = frontmatter::split(&content)
        .with_context(|| format!("splitting frontmatter for {}", path.display()))?;
    let frontmatter: FactFrontmatter =
        serde_yaml::from_str(&fm_text).context("parsing fact frontmatter")?;
    Ok(Fact {
        frontmatter,
        body: body.trim().to_string(),
    })
}

pub fn load_by_id(facts_root: &Path, id: &str) -> Result<Fact> {
    // id like "object.teapot" → facts/object/teapot.md
    let parts: Vec<&str> = id.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "fact id must be of form '<type>.<name>': got {id}"
        ));
    }
    let path = facts_root.join(parts[0]).join(format!("{}.md", parts[1]));
    load(&path)
}
