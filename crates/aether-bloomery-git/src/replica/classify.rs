//! Classify a `git push --porcelain` failure onto [`ReplicaError`].

use std::process::Output;

use super::ReplicaError;

/// One `git push --porcelain` per-ref status line.
struct PorcelainRef {
    flag: char,
    dst: String,
    summary: String,
}

pub(super) fn classify_push(output: &Output, mainline: &str) -> Result<(), ReplicaError> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let refs = porcelain_refs(&stdout);
    let detail = push_detail(&refs, &stderr, &stdout);

    if refs.iter().any(|git_ref| git_ref.flag == '!' && git_ref.dst == mainline) {
        return Err(ReplicaError::ForceRejected(detail));
    }
    if refs.iter().any(|git_ref| git_ref.flag == '!') {
        return Err(ReplicaError::Deterministic(detail));
    }

    let lower = format!("{stderr}{stdout}").to_ascii_lowercase();
    if is_auth(&lower) || is_missing_refspec(&lower) || is_unknown_remote(&lower) {
        Err(ReplicaError::Deterministic(detail))
    } else {
        Err(ReplicaError::Transient(detail))
    }
}

fn porcelain_refs(stdout: &str) -> Vec<PorcelainRef> {
    stdout.lines().filter_map(parse_porcelain_ref).collect()
}

fn parse_porcelain_ref(line: &str) -> Option<PorcelainRef> {
    let (flag_field, rest) = line.split_once('\t')?;
    if flag_field.len() != 1 {
        return None;
    }
    let flag = flag_field.chars().next()?;
    let (spec, summary) = rest.split_once('\t')?;
    let dst = spec.split_once(':').map_or(spec, |(_, dst)| dst);
    Some(PorcelainRef { flag, dst: dst.to_owned(), summary: summary.trim().to_owned() })
}

fn push_detail(refs: &[PorcelainRef], stderr: &str, stdout: &str) -> String {
    let stderr = stderr.trim();
    if refs.is_empty() {
        return if stderr.is_empty() {
            stdout.trim().to_owned()
        } else if stdout.trim().is_empty() {
            stderr.to_owned()
        } else {
            format!("{stderr}\n{}", stdout.trim())
        };
    }
    let report = refs.iter().map(render_ref).collect::<Vec<_>>().join("; ");
    if stderr.is_empty() {
        report
    } else {
        format!("{report}\n{stderr}")
    }
}

fn render_ref(git_ref: &PorcelainRef) -> String {
    match git_ref.flag {
        '!' => format!("{} rejected ({})", git_ref.dst, git_ref.summary),
        '=' => format!("{} up to date", git_ref.dst),
        ' ' | '+' | '*' | '-' => format!("{} delivered ({})", git_ref.dst, git_ref.summary),
        other => format!("{} '{other}' ({})", git_ref.dst, git_ref.summary),
    }
}

fn is_auth(lower: &str) -> bool {
    lower.contains("invalid credentials")
        || lower.contains("authentication failed")
        || lower.contains("invalid username or password")
        || lower.contains("could not read username")
}

fn is_missing_refspec(lower: &str) -> bool {
    lower.contains("src refspec") || lower.contains("does not match any") || lower.contains("invalid refspec")
}

fn is_unknown_remote(lower: &str) -> bool {
    lower.contains("does not appear to be a git repository")
}
