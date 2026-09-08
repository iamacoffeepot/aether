//! Git subprocesses. Every revision read goes through `git show` / `git grep` / `git diff`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn repo_root() -> Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("spawn git rev-parse")?;
    if !out.status.success() {
        bail!(
            "git rev-parse --show-toplevel failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().into())
}

pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.replace("\r\n", "\n"))
}

pub fn run_raw(repo: &Path, args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    Ok((code, stdout, stderr))
}

pub fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?
        .trim()
        .to_string())
}

pub fn parent_of(repo: &Path, rev: &str) -> Result<String> {
    Ok(run(repo, &["rev-parse", "--verify", &format!("{rev}^")])?
        .trim()
        .to_string())
}

/// `git diff --name-only base head -- '*.rs'`, kept to `crates/` and `xtask/`.
pub fn changed_rs_files(repo: &Path, base: &str, head: &str, prefixes: &[String]) -> Result<Vec<String>> {
    let text = run(repo, &["diff", "--name-only", base, head, "--", "*.rs"])?;
    let mut files: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|p| p.starts_with("crates/") || p.starts_with("xtask/"))
        .map(str::to_string)
        .collect();
    if !prefixes.is_empty() {
        files.retain(|p| prefixes.iter().any(|pre| p.starts_with(pre)));
    }
    files.sort();
    files.dedup();
    Ok(files)
}

pub fn show_file(repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{rev}:{path}");
    let (code, stdout, stderr) = run_raw(repo, &["show", &spec])?;
    if code == 0 {
        return Ok(Some(stdout));
    }
    let err = stderr.to_ascii_lowercase();
    if code == 128
        && (err.contains("does not exist")
            || err.contains("exists on disk, but not in")
            || err.contains("bad revision")
            || err.contains("invalid object")
            || err.contains("pathspec"))
    {
        return Ok(None);
    }
    bail!("git show {spec} failed ({code}): {}", stderr.trim());
}

/// `git grep -n -w -F <name> <rev> -- '*.rs'`
pub fn grep_ident(repo: &Path, rev: &str, name: &str) -> Result<Vec<GrepHit>> {
    let (code, stdout, stderr) = run_raw(repo, &["grep", "-n", "-w", "-F", name, rev, "--", "*.rs"])?;
    if code == 1 && stdout.is_empty() {
        return Ok(Vec::new());
    }
    if code != 0 {
        bail!("git grep -n -w -F {name} {rev} failed ({code}): {}", stderr.trim());
    }
    let mut hits = Vec::new();
    for line in stdout.lines() {
        if let Some(hit) = parse_grep_line(rev, line) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

#[derive(Clone, Debug)]
pub struct GrepHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

fn parse_grep_line(rev: &str, line: &str) -> Option<GrepHit> {
    let rest = line.strip_prefix(&format!("{rev}:")).unwrap_or(line);
    let (path, rest) = rest.split_once(':')?;
    let (line_no, text) = rest.split_once(':')?;
    let line = line_no.parse().ok()?;
    Some(GrepHit {
        path: path.to_string(),
        line,
        text: text.to_string(),
    })
}

pub fn cargo_package_from_path(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("crates/") {
        rest.split('/').next().map(str::to_string)
    } else if path.starts_with("xtask/") {
        Some("xtask".into())
    } else {
        None
    }
}
