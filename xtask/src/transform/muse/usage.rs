//! Muse's token counts, read from the session log it persists.
//!
//! They are absent from `muse exec --json` stdout, which is the only stream the
//! arm reads, so the counts have to be fetched from the log Muse writes
//! alongside the run. Every stdout record carries the session id in `stream.id`,
//! and the arm already captures that stdout, so locating the log costs nothing
//! at the call site and Muse's own invocation is unchanged.

use std::path::{Path, PathBuf};
use std::{env, fs};

use crate::transform::lane::Usage;

/// The `YYYY/MM/DD` levels between `sessions` and a session's own directory —
/// the walk's bound, so a large history costs a directory listing per level
/// rather than a full tree traversal.
const DATE_DEPTH: usize = 3;

/// The session id `muse exec --json` stamps on every record it emits.
///
/// Read off the first record rather than the terminal: a run that dies early
/// still names its session, so a partial run's tokens are still recoverable.
pub(super) fn session_id(transcript: &str) -> Option<String> {
    transcript.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line).ok()?.get("stream")?.get("id")?.as_str().map(str::to_owned)
    })
}

/// Total the tokens Muse recorded for `session_id`, across the run and every
/// subagent it spawned.
///
/// `None` when the log cannot be found or read. That is deliberately not a
/// zeroed `Usage`: `record` renders `None` as null columns, and a study must
/// read an unreadable log as unmeasured rather than as a free attempt.
pub(super) fn from_session_log(session_id: &str) -> Option<Usage> {
    let session = session_dir(&data_root()?, session_id)?;

    // The parent log holds only the main agent's steps; each subagent gets its
    // own. A total that read the parent alone would understate a fan-out run,
    // in the one direction that flatters the cheap lane.
    let mut total = Usage { input: 0, cache_read: 0, cache_write: 0, output: 0 };
    add_log(&session.join("session.jsonl"), &mut total);
    for subagent in read_dir_sorted(&session.join("subagent")) {
        add_log(&subagent.join("session.jsonl"), &mut total);
    }
    Some(total)
}

/// Muse's data root: `$XDG_DATA_HOME/muse`, else `$HOME/.local/share/muse`.
///
/// Muse honours `XDG_DATA_HOME`, so a caller that redirects it gets a log the
/// arm still finds.
#[allow(clippy::disallowed_methods)] // XDG/HOME are external vars, not cap config.
fn data_root() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("muse"));
    }
    Some(PathBuf::from(env::var("HOME").ok()?).join(".local/share/muse"))
}

/// Find `sessions/*/*/*/<session_id>` under `root`.
///
/// The date directories are walked rather than computed: a run that starts
/// before midnight and is read after it would miss on a date computed at read
/// time, and the walk is three shallow listings.
fn session_dir(root: &Path, session_id: &str) -> Option<PathBuf> {
    let mut days = vec![root.join("sessions")];
    for _ in 0..DATE_DEPTH {
        days = days.iter().flat_map(|dir| read_dir_sorted(dir)).collect();
    }

    days.iter().flat_map(|day| read_dir_sorted(day)).find(|dir| dir.file_name().is_some_and(|name| name == session_id))
}

/// The subdirectories of `dir`, sorted, or empty when it cannot be read.
///
/// Sorted so a total is assembled in the same order twice — the sum does not
/// depend on it, but a log dump beside it reads the same on a re-run.
fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    entries
}

/// Add every `model_completed` step in the log at `path` into `total`.
///
/// A missing or malformed log contributes nothing rather than failing the run:
/// the attempt itself succeeded, and losing its cost row must not turn a
/// completed run into a failed one.
fn add_log(path: &Path, total: &mut Usage) {
    if let Ok(log) = fs::read_to_string(path) {
        add_steps(&log, total);
    }
}

/// Add every `model_completed` step in the `log` text into `total`.
fn add_steps(log: &str, total: &mut Usage) {
    for step in log.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()) {
        let Some(usage) = step.pointer("/payload/event/usage").filter(|_| {
            step.pointer("/payload/event/kind").and_then(serde_json::Value::as_str) == Some("model_completed")
        }) else {
            continue;
        };
        let count = |field: &str| usage.get(field).and_then(serde_json::Value::as_u64).unwrap_or(0);

        // Muse's `input_tokens` counts the whole prompt, cached tokens included,
        // so the cache read is subtracted to leave the uncached input the other
        // arms report — pricing the two apart is the point of the split, and
        // folding them would bill every cached token at the uncached rate.
        // `reasoning_tokens` is left alone: it is a subset of `output_tokens`,
        // so adding it would double-count.
        total.input += count("input_tokens").saturating_sub(count("cache_read_tokens"));
        total.cache_read += count("cache_read_tokens");
        total.cache_write += count("cache_write_tokens");
        total.output += count("output_tokens");
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::{env, fs, process};

    use super::{add_log, add_steps, session_dir, session_id};
    use crate::transform::lane::Usage;

    /// A per-test scratch directory under the system temp dir, unique per call
    /// so concurrent test threads never collide.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-muse-usage-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn the_session_id_comes_off_the_first_record_so_a_died_early_run_still_names_its_log() {
        let transcript = concat!(
            r#"{"stream":{"kind":"session","id":"2a2aeda2-6f38-4462-b519-2bf30e59a52e"},"payload_type":"run.lifecycle.started"}"#,
            "\n",
            r#"{"stream":{"kind":"session","id":"2a2aeda2-6f38-4462-b519-2bf30e59a52e"},"payload_type":"run.terminal.completed"}"#,
        );
        assert_eq!(session_id(transcript).as_deref(), Some("2a2aeda2-6f38-4462-b519-2bf30e59a52e"));

        assert_eq!(session_id("").as_deref(), None);
        assert_eq!(session_id("not json").as_deref(), None);
    }

    // Tripwire: the arithmetic over a real pair of `model_completed` steps.
    // Muse's `input_tokens` includes the cached tokens, so `input` must be the
    // uncached remainder — 20475 + (21450 - 20465) — and not the raw sum, which
    // would price 20465 cached tokens at the uncached rate.
    #[test]
    fn a_steps_cached_input_is_not_counted_as_uncached_input() {
        let log = concat!(
            r#"{"payload":{"event":{"kind":"model_completed","usage":{"input_tokens":20475,"output_tokens":955,"cached_tokens":0,"cache_write_tokens":0,"cache_read_tokens":0,"reasoning_tokens":474}}}}"#,
            "\n",
            r#"{"payload":{"event":{"kind":"model_completed","usage":{"input_tokens":21450,"output_tokens":289,"cached_tokens":20465,"cache_write_tokens":0,"cache_read_tokens":20465,"reasoning_tokens":63}}}}"#,
            "\n",
            // Not a model step: its tokens are attribution bookkeeping over the
            // steps above, so counting it would double the run.
            r#"{"payload":{"event":{"kind":"goal_usage_attribution","usage":{"input_tokens":99999,"output_tokens":99999}}}}"#,
        );

        let mut total = Usage { input: 0, cache_read: 0, cache_write: 0, output: 0 };
        add_steps(log, &mut total);

        assert_eq!(total.input, 21460, "uncached input only: 20475 + (21450 - 20465)");
        assert_eq!(total.cache_read, 20465);
        assert_eq!(total.output, 1244, "955 + 289, with reasoning left out as a subset of output");
        assert_eq!(total.cache_write, 0, "a reported zero is a zero");
    }

    // A log that cannot be read leaves the total untouched, so a run whose log
    // is missing records unmeasured rather than a fabricated zero.
    #[test]
    fn an_unreadable_log_contributes_nothing() {
        let mut total = Usage { input: 7, cache_read: 0, cache_write: 0, output: 3 };
        add_log(Path::new("/nonexistent/session.jsonl"), &mut total);
        assert_eq!((total.input, total.output), (7, 3));
    }

    // The date directories are walked, not computed, so a session written
    // yesterday is still found today.
    #[test]
    fn the_session_directory_is_found_under_any_date() {
        let root = scratch_dir("session-dir");
        let session = root.join("sessions/2026/08/07/abc-123");
        fs::create_dir_all(&session).expect("create session dir");

        assert_eq!(session_dir(&root, "abc-123").as_deref(), Some(session.as_path()));
        assert_eq!(session_dir(&root, "missing"), None);
    }
}
