//! What a proof run says to the model: the files `cargo fmt` rewrote, the
//! diagnostics of the step that failed, and the summary that carries both,
//! capped so one call output stays readable.

use std::collections::BTreeSet;
use std::iter;

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
    /// `cargo fmt` could not format the tree, so the cargo step did not run.
    FmtFailed,
    /// The cargo step failed.
    CargoFailed,
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
/// lines. `level` keeps only that rustc level (`error` for a test proof's
/// build errors); `None` keeps every level, as the clippy proof does.
#[must_use]
pub fn rendered(stdout: &[u8], level: Option<&str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-message")
        .filter(|message| level.is_none_or(|want| message["message"]["level"] == want))
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

/// The diagnostics of a failed clippy step from its raw outputs.
#[must_use]
pub fn clippy_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    diagnostics(&rendered(stdout, None), stderr)
}

/// Cargo's failed-target list from `stderr`: the `error: N targets failed:`
/// header and the `` `-p …` `` lines under it, in the order cargo printed
/// them.
fn failed_targets(stderr: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(stderr);
    let mut lines = text.lines().skip_while(|line| !is_failed_targets(line));
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    iter::once(header)
        .chain(lines.take_while(|line| line.trim_start().starts_with('`')))
        .map(|line| line.trim_end().to_owned())
        .collect()
}

/// Whether `line` is cargo's `error: N target(s) failed:` header.
fn is_failed_targets(line: &str) -> bool {
    line.starts_with("error: ") && line.ends_with(" failed:")
}

/// Whether `line` is a JSON message cargo printed on `stdout`, which the test
/// failure text skips.
fn is_cargo_message(line: &str) -> bool {
    line.starts_with("{\"reason\":")
}

/// Whether `trimmed` opens a libtest failure block.
fn is_failure_header(trimmed: &str) -> bool {
    trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----")
}

/// Whether `trimmed` is a libtest result line, passing or failing.
fn is_test_result(trimmed: &str) -> bool {
    trimmed.starts_with("test result: ")
}

/// Whether `trimmed` ends the failure block it follows: the next block, the
/// `failures:` list of names, or the binary's result line.
fn ends_block(trimmed: &str) -> bool {
    is_failure_header(trimmed) || trimmed == "failures:" || is_test_result(trimmed)
}

/// Each libtest failure block (`---- … stdout ----` through the end of its
/// output) and its binary's `test result: FAILED` line, in the order they
/// were printed, with cargo's JSON lines skipped.
fn failed_blocks(stdout: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(stdout);
    let mut parts = Vec::new();
    let mut block: Option<Vec<&str>> = None;
    for line in text.lines().filter(|line| !is_cargo_message(line)) {
        let trimmed = line.trim();
        if ends_block(trimmed) {
            parts.extend(block.take().map(|lines| lines.join("\n").trim_end().to_owned()));
        }
        let failed = is_test_result(trimmed) && trimmed.contains("FAILED");
        if is_failure_header(trimmed) {
            block = Some(vec![line]);
        } else if let Some(lines) = block.as_mut() {
            lines.push(line.trim_end());
        } else if failed {
            parts.push(trimmed.to_owned());
        }
    }
    parts.extend(block.map(|lines| lines.join("\n").trim_end().to_owned()));
    parts
}

/// The test failure text: cargo's failed-target lines from `stderr` first,
/// then each libtest failure block and its binary's `test result: FAILED`
/// line from `stdout`, in the order they were printed. The target list goes
/// first because [`capped`] keeps the head and cuts the tail.
#[must_use]
pub fn test_failures(stdout: &[u8], stderr: &[u8]) -> String {
    let mut parts = Vec::new();
    let targets = failed_targets(stderr);
    if !targets.is_empty() {
        parts.push(targets.join("\n"));
    }
    parts.extend(failed_blocks(stdout));
    parts.join("\n\n")
}

/// The diagnostics of a failed test step from its raw outputs: the build
/// errors when the build failed, else the test failures, else the step's
/// `stderr`, as the clippy proof falls back to. Capped at
/// [`DIAGNOSTICS_MAX_BYTES`].
#[must_use]
pub fn test_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let build = rendered(stdout, Some("error"));
    if !build.is_empty() {
        return capped(build.join("\n\n"));
    }
    let failures = test_failures(stdout, stderr);
    if !failures.trim().is_empty() {
        return capped(failures);
    }
    capped(String::from_utf8_lossy(stderr).trim_end().to_owned())
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
/// diagnostics of a failed step after a blank line. `step` is the cargo
/// step's summary name: `cargo clippy` or `cargo test`.
#[must_use]
pub fn summary(ended: &Ended, step: &str, formatted: &[String], diagnostics: Option<&str>) -> String {
    let verdict = match ended {
        Ended::Passed => format!("`{step}` passed."),
        Ended::FmtFailed => format!("`cargo fmt` failed, so `{step}` did not run."),
        Ended::CargoFailed => format!("`{step}` failed."),
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
    use super::{DIAGNOSTICS_MAX_BYTES, capped, test_diagnostics, test_failures};

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

    #[test]
    fn test_diagnostics_list_targets_first_then_each_failure_block_once_in_order() {
        // Catches dropped blocks, JSON noise, a target list lost to the cap, a build error read as a test
        // failure, and a failure with no blocks falling back to nothing.
        let binary = |name: &str, stdout: &str| format!("     Running unittests src/lib.rs ({name})\n{stdout}\n");
        let warning = r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"warning: unused\n"}}"#;
        let error = r#"{"reason":"compiler-message","message":{"level":"error","rendered":"error: expected one of `!` or `::`\n"}}"#;
        let passing = "test result: ok. 1 passed; 0 failed;";
        let first = "---- one stdout ----\nthread 'one' panicked at src/lib.rs:1\nnote: run again\n\ntest result: FAILED. 0 passed; 1 failed;";
        let doctest =
            "---- src/lib.rs - f (line 1) stdout ----\nassertion failed\n\ntest result: FAILED. 0 passed; 1 failed;";
        let stdout = format!(
            "{}\n{}{}\n{}\n{}\n{}\n{}",
            binary("passing", passing),
            binary("failing", first),
            warning,
            r#"{"reason":"compiler-artifact","filenames":[]}"#,
            r#"{"reason":"build-finished","success":false}"#,
            binary("doctest", doctest),
            "failures:\n    one\n",
        );
        let stderr =
            "error: 2 targets failed:\n    `-p aether-demo --test demo`\n    `-p aether-demo --doctest demo`\n";
        let diagnostics = test_diagnostics(stdout.as_bytes(), stderr.as_bytes());
        assert!(
            diagnostics.starts_with("error: 2 targets failed:"),
            "the target list goes first so the cap keeps it: {diagnostics}"
        );
        for want in [
            "`-p aether-demo --test demo`",
            "`-p aether-demo --doctest demo`",
            "---- one stdout ----\nthread 'one' panicked at src/lib.rs:1\nnote: run again",
            "test result: FAILED. 0 passed; 1 failed;",
            "---- src/lib.rs - f (line 1) stdout ----\nassertion failed",
        ] {
            assert!(diagnostics.contains(want), "the diagnostics hold {want:?}: {diagnostics}");
        }
        assert_eq!(diagnostics.matches("test result: FAILED").count(), 2, "{diagnostics}");
        assert!(!diagnostics.contains("compiler-artifact"), "{diagnostics}");
        assert!(!diagnostics.contains("warning: unused"), "a warning is no test diagnostic: {diagnostics}");
        assert!(
            diagnostics.find("---- one stdout ----").expect("first block")
                < diagnostics.find("---- src/lib.rs - f (line 1) stdout ----").expect("doctest block"),
            "{diagnostics}"
        );

        let built = test_diagnostics(error.as_bytes(), b"error: build failed\n");
        assert_eq!(built, "error: expected one of `!` or `::`");

        let bare = test_diagnostics(b"", b"error: could not compile\n");
        assert_eq!(bare, "error: could not compile");
    }

    #[test]
    fn test_failures_hold_the_targets_then_the_blocks() {
        // Catches the target lines and the failure blocks joined out of order or dropped.
        let stdout = "---- one stdout ----\nboom\ntest result: FAILED. 0 passed; 1 failed;\n";
        let stderr = "error: 1 target failed:\n    `-p aether-demo --test demo`\n";
        assert_eq!(
            test_failures(stdout.as_bytes(), stderr.as_bytes()),
            "error: 1 target failed:\n    `-p aether-demo --test demo`\n\n---- one stdout ----\nboom\n\ntest result: FAILED. 0 passed; 1 failed;"
        );
        assert_eq!(test_failures(b"", b""), "");
    }
}
