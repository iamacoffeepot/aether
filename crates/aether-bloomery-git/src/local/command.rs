//! One place that spawns `git` and classifies what came back.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::client::GitDataError;

/// The oldest git that ships `merge-tree --write-tree`. Fail boot below this
/// rather than building a temporary-index fallback.
pub const MIN_GIT: (u32, u32, u32) = (2, 38, 0);

/// Ask the installed `git` what it is.
///
/// # Errors
/// The binary could not be spawned or did not print a parseable version.
pub fn detect_version() -> Result<(u32, u32, u32), GitDataError> {
    let output = Command::new("git")
        .arg("version")
        .output()
        .map_err(|error| GitDataError::Command(format!("spawning git version: {error}")))?;
    if !output.status.success() {
        return Err(GitDataError::Command(format!("git version: {}", String::from_utf8_lossy(&output.stderr).trim())));
    }
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `git version` stdout (`git version 2.43.0`, optionally with a suffix).
///
/// # Errors
/// The text is not a `git version A.B[.C]` line.
pub fn parse_version(stdout: &str) -> Result<(u32, u32, u32), GitDataError> {
    let rest = stdout
        .trim()
        .strip_prefix("git version ")
        .ok_or_else(|| GitDataError::Command(format!("unrecognised git version output: {}", stdout.trim())))?;
    let token = rest.split_whitespace().next().unwrap_or("");
    let mut parts = token.split('.');
    let major = parse_component(parts.next(), token)?;
    let minor = parse_component(parts.next(), token)?;
    let patch = parts.next().map_or(0, |part| leading_digits(part).parse().unwrap_or(0));
    Ok((major, minor, patch))
}

fn parse_component(part: Option<&str>, token: &str) -> Result<u32, GitDataError> {
    part.and_then(|part| leading_digits(part).parse().ok())
        .ok_or_else(|| GitDataError::Command(format!("unrecognised git version token: {token}")))
}

fn leading_digits(part: &str) -> String {
    part.chars().take_while(char::is_ascii_digit).collect()
}

/// Refuse a git older than [`MIN_GIT`], naming the version we found.
///
/// # Errors
/// `have` is below the merge-tree floor.
pub fn require_min(have: (u32, u32, u32)) -> Result<(), GitDataError> {
    if have < MIN_GIT {
        return Err(GitDataError::Command(format!(
            "git 2.38 or newer is required for merge-tree --write-tree; found {}.{}.{}",
            have.0, have.1, have.2
        )));
    }
    Ok(())
}

/// Run `git -C repo args…` and return the raw output.
///
/// # Errors
/// The process could not be spawned.
pub fn run(repo: &Path, args: &[&str]) -> Result<Output, GitDataError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| GitDataError::Command(format!("git {args:?} in {}: {error}", repo.display())))
}

/// Run `git -C repo args…` and return trimmed stdout on success.
///
/// # Errors
/// Spawn failed or the command exited non-zero.
pub fn run_ok(repo: &Path, args: &[&str]) -> Result<String, GitDataError> {
    let output = run(repo, args)?;
    if !output.status.success() {
        return Err(GitDataError::Command(format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Run `git -C repo args…` with extra environment (commit identity).
///
/// # Errors
/// The process could not be spawned.
pub fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Output, GitDataError> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .envs(envs.iter().copied())
        .args(args)
        .output()
        .map_err(|error| GitDataError::Command(format!("git {args:?} in {}: {error}", repo.display())))
}

/// Run `git -C repo args…` with `stdin` piped in.
///
/// # Errors
/// Spawn, stdin write, or wait failed.
pub fn run_stdin(repo: &Path, args: &[&str], stdin: &str) -> Result<Output, GitDataError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitDataError::Command(format!("git {args:?} in {}: {error}", repo.display())))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes()).map_err(|error| {
            GitDataError::Command(format!("writing git {args:?} stdin in {}: {error}", repo.display()))
        })?;
    }
    child
        .wait_with_output()
        .map_err(|error| GitDataError::Command(format!("git {args:?} in {}: {error}", repo.display())))
}

/// Classify a failed `update-ref` (single or `--stdin`) onto the git-data
/// vocabulary. Git's wording is the only signal — there is no status code.
#[must_use]
pub fn classify_update(output: &Output, name: &str) -> GitDataError {
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    let lower = detail.to_ascii_lowercase();
    if lower.contains("already exists") || lower.contains("but expected") || lower.contains("is at") {
        GitDataError::RefConflict(format!("{name}: {detail}"))
    } else if lower.contains("unable to resolve")
        || lower.contains("not a valid object")
        || lower.contains("bad object")
        || lower.contains("missing")
    {
        GitDataError::MissingObject(format!("{name}: {detail}"))
    } else {
        GitDataError::Command(format!("git update-ref {name}: {detail}"))
    }
}

/// Whether `stderr` from `update-ref -d` means the name was already gone.
#[must_use]
pub fn is_absent_delete(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("unable to resolve") || (lower.contains("cannot lock ref") && lower.contains("unable to delete"))
}

#[cfg(test)]
mod tests {
    use super::{MIN_GIT, parse_version, require_min};

    #[test]
    fn parse_version_reads_plain_and_suffixed_git_version_lines() {
        assert_eq!(parse_version("git version 2.43.0\n").unwrap(), (2, 43, 0));
        assert_eq!(parse_version("git version 2.39.5 (Apple Git-154)").unwrap(), (2, 39, 5));
        assert_eq!(parse_version("git version 2.38.0").unwrap(), MIN_GIT);
    }

    #[test]
    fn require_min_names_a_too_old_version() {
        let error = require_min((2, 37, 0)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("2.38"), "{text}");
        assert!(text.contains("2.37.0"), "{text}");
    }
}
