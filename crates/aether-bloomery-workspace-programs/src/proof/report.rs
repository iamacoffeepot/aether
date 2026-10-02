//! What a proof run says to the model: the files `cargo fmt` rewrote, the
//! diagnostics of the step that failed, and the summary that carries both,
//! capped so one call output stays readable.

use std::collections::BTreeSet;

use serde_json::Value;

/// The most bytes of diagnostics one proof reports: 64 KiB, the closing cut
/// line included.
pub const DIAGNOSTICS_MAX_BYTES: usize = 64 * 1024;

/// The bytes kept back from the diagnostics budget for the closing cut line.
const CUT_BYTES: usize = 128;

/// The most rewritten files a summary names before it counts the rest.
const MAX_NAMED_FILES: usize = 32;

/// Where the run's tree is written; rustfmt prints absolute paths under it.
const WORK_PREFIX: &str = "/work/";

/// How a proof run ended.
pub enum Ended {
    /// Every step exited 0.
    Passed,
    /// `cargo fmt` could not format the tree, so clippy did not run.
    FmtFailed,
    /// `cargo clippy` failed.
    ClippyFailed,
}

/// The files `cargo fmt -- -l` listed on `stdout`, relative to the tree's
/// root, in the order it printed them.
#[must_use]
pub fn formatted(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_prefix(WORK_PREFIX).unwrap_or(line).to_owned())
        .collect()
}

/// The `rendered` text of every `compiler-message` cargo printed as a JSON
/// line on `stdout`, each once, in the order cargo printed them. A line that
/// is not such a message is skipped: cargo interleaves artifact and build
/// lines.
#[must_use]
pub fn rendered(stdout: &[u8]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-message")
        .filter_map(|message| message["message"]["rendered"].as_str().map(|text| text.trim_end().to_owned()))
        .filter(|text| seen.insert(text.clone()))
        .collect()
}

/// The diagnostics of a failed step: what cargo rendered, or, when it
/// rendered nothing, the step's `stderr`, where cargo's own errors and
/// rustfmt's parse errors go. Capped at [`DIAGNOSTICS_MAX_BYTES`].
#[must_use]
pub fn diagnostics(rendered: &[String], stderr: &[u8]) -> String {
    let text = if rendered.is_empty() {
        String::from_utf8_lossy(stderr).trim_end().to_owned()
    } else {
        rendered.join("\n\n")
    };
    capped(text)
}

/// `text` cut at the last line that fits [`DIAGNOSTICS_MAX_BYTES`] with its
/// closing cut line, which says how much was cut.
fn capped(text: String) -> String {
    if text.len() <= DIAGNOSTICS_MAX_BYTES {
        return text;
    }
    let budget = DIAGNOSTICS_MAX_BYTES - CUT_BYTES;
    let boundary = (0..=budget).rev().find(|&index| text.is_char_boundary(index)).unwrap_or(0);
    let kept = text[..boundary].rfind('\n').unwrap_or(boundary);
    let cut = text.len() - kept;
    format!("{}\n[... {cut} more bytes of diagnostics cut]", &text[..kept])
}

/// What the model reads: how the proof ended, what fmt rewrote, and the
/// diagnostics of a failed step after a blank line.
#[must_use]
pub fn summary(ended: &Ended, formatted: &[String], diagnostics: Option<&str>) -> String {
    let verdict = match ended {
        Ended::Passed => "`cargo clippy` passed with no warning.",
        Ended::FmtFailed => "`cargo fmt` failed, so `cargo clippy` did not run.",
        Ended::ClippyFailed => "`cargo clippy` failed.",
    };
    let rewrote = rewrote(formatted);
    let reported = diagnostics.map(|diagnostics| format!("\n\n{diagnostics}")).unwrap_or_default();
    format!("{verdict} {rewrote}{reported}")
}

/// One sentence naming the files fmt rewrote, at most [`MAX_NAMED_FILES`]
/// of them.
fn rewrote(formatted: &[String]) -> String {
    if formatted.is_empty() {
        return "`cargo fmt` changed nothing.".to_owned();
    }
    let named = formatted.iter().take(MAX_NAMED_FILES).map(String::as_str).collect::<Vec<_>>().join(", ");
    let more = formatted.len().saturating_sub(MAX_NAMED_FILES);
    let rest = if more == 0 {
        String::new()
    } else {
        format!(" and {more} more")
    };
    format!("`cargo fmt` rewrote {named}{rest}; read a rewritten file again before you edit it.")
}

#[cfg(test)]
mod tests {
    use super::{DIAGNOSTICS_MAX_BYTES, capped};

    #[test]
    fn diagnostics_past_the_cap_are_cut_at_a_line_and_marked() {
        // Catches an uncapped text that floods the model's context, a cut inside a line or a character, and a cut
        // the model cannot see.
        let line = "é".repeat(99);
        let text = vec![line.as_str(); 1000].join("\n");
        let cut = capped(text.clone());
        assert!(cut.len() <= DIAGNOSTICS_MAX_BYTES, "{}", cut.len());
        let (kept, marker) = cut.rsplit_once('\n').expect("a cut line");
        assert!(text.starts_with(kept) && kept.ends_with(&line), "cut at a line end");
        assert_eq!(marker, format!("[... {} more bytes of diagnostics cut]", text.len() - kept.len()));
        assert_eq!(capped("short".to_owned()), "short");
    }
}
