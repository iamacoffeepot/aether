use anyhow::{Result, anyhow};

pub fn split(content: &str) -> Result<(String, String)> {
    let normalized = content.replace("\r\n", "\n");
    let mut iter = normalized.splitn(3, "---\n");
    let before = iter.next().ok_or_else(|| anyhow!("empty content"))?;
    let fm = iter
        .next()
        .ok_or_else(|| anyhow!("missing opening '---' delimiter"))?;
    let body = iter
        .next()
        .ok_or_else(|| anyhow!("missing closing '---' delimiter"))?;
    if !before.is_empty() {
        return Err(anyhow!("content before opening '---' delimiter"));
    }
    Ok((fm.to_string(), body.to_string()))
}
