use anyhow::{Context, Result, anyhow};
use std::process::Command;

pub fn complete(prompt: &str, model: &str) -> Result<String> {
    let output = Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .args(["--model", model])
        .args(["--max-turns", "1"])
        .args(["--output-format", "text"])
        .output()
        .context("spawning claude subprocess (is the `claude` CLI installed and on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "claude failed (status {}): {}",
            output.status,
            truncate(&stderr, 800)
        ));
    }

    let stdout = String::from_utf8(output.stdout).context("claude stdout not valid utf-8")?;
    Ok(stdout.trim().to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max).collect();
        format!("{clipped}...")
    }
}
