use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::frontmatter;

#[derive(Debug, Clone, Deserialize)]
pub struct ObserverFrontmatter {
    pub id: String,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct Observer {
    pub frontmatter: ObserverFrontmatter,
    pub body: String,
}

pub fn load(path: &Path) -> Result<Observer> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading observer {}", path.display()))?;
    let (fm_text, body) = frontmatter::split(&content)
        .with_context(|| format!("splitting frontmatter for {}", path.display()))?;
    let frontmatter: ObserverFrontmatter =
        serde_yaml::from_str(&fm_text).context("parsing observer frontmatter")?;
    Ok(Observer {
        frontmatter,
        body: body.trim().to_string(),
    })
}

pub fn load_by_id(observers_root: &Path, id: &str) -> Result<Observer> {
    // id like "observer.quiet-domestic" → observers/quiet-domestic.md
    let name = id.strip_prefix("observer.").unwrap_or(id);
    let path = observers_root.join(format!("{name}.md"));
    load(&path)
}
