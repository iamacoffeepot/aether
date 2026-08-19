//! The argv a lane dispatch hands its program.
//!
//! Parsed by hand rather than through clap: the point of the mock is to see
//! exactly what [`ProcessTransformRunner`] built, so an unknown flag is recorded
//! and ignored instead of aborting the run. A parser that refused new flags
//! would turn every argv addition into a mock failure rather than a scenario
//! that keeps running while the ledger shows the flag arrived.
//!
//! [`ProcessTransformRunner`]: super::super::ProcessTransformRunner

use std::path::PathBuf;
use std::{error, fmt};

/// A lane dispatch's argv, as the mock reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneArgs {
    /// The positional transform command id.
    pub command: String,
    /// `--out`: where `evidence.json` goes.
    pub out: PathBuf,
    /// `--nonce`: the idempotency nonce.
    pub nonce: String,
    /// `--subject`: the commit the lane runs against (model lanes only).
    pub subject: Option<String>,
    /// `--diff-base`: the committed base a candidate is judged against, absent
    /// for the working-tree contract a member lane runs under.
    pub diff_base: Option<String>,
    /// `--task`: the advisory work order.
    pub task: Option<String>,
}

/// What a mis-shaped argv leaves the mock unable to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgvError {
    /// No positional command id.
    MissingCommand,
    /// No `--out`, so there is nowhere to write evidence and no script to read.
    MissingOut,
    /// A flag that takes a value arrived last, with none.
    DanglingValue(String),
}

impl fmt::Display for ArgvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => f.write_str("no positional transform command id"),
            Self::MissingOut => f.write_str("no --out directory"),
            Self::DanglingValue(flag) => write!(f, "{flag} arrived with no value"),
        }
    }
}

impl error::Error for ArgvError {}

/// Parse the arguments a dispatch appended after the lane program's own leading
/// words.
///
/// # Errors
/// The argv carries no command id or no `--out`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<LaneArgs, ArgvError> {
    let mut parsed = LaneArgs::default();
    let mut command = None;
    let mut out = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| ArgvError::DanglingValue(arg.clone()));
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(value()?)),
            "--nonce" => parsed.nonce = value()?,
            "--subject" => parsed.subject = Some(value()?),
            "--diff-base" => parsed.diff_base = Some(value()?),
            "--task" => parsed.task = Some(value()?),
            // The axes the mock records but does not act on. Consuming their
            // values keeps them from being read as the positional command.
            "--harness" | "--model" | "--effort" | "--resume" | "--seeded" => {
                value()?;
            }
            other if other.starts_with("--") => {}
            _ => command = Some(arg),
        }
    }

    parsed.command = command.ok_or(ArgvError::MissingCommand)?;
    parsed.out = out.ok_or(ArgvError::MissingOut)?;
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a parser test that cannot parse its own fixture reports it by panicking")]
mod tests {
    use std::path::Path;

    use super::{ArgvError, parse};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_model_lane_dispatch_parses_every_axis_the_coordinator_threaded() {
        let args = parse(argv(&[
            "review.critic",
            "--out",
            "/tmp/n-1-evidence",
            "--nonce",
            "n-1",
            "--subject",
            "abc123",
            "--diff-base",
            "def456",
            "--harness",
            "claude",
            "--model",
            "claude-opus-5",
            "--effort",
            "high",
            "--task",
            "close the gap",
        ]))
        .unwrap();

        assert_eq!(args.command, "review.critic");
        assert_eq!(args.out, Path::new("/tmp/n-1-evidence"));
        assert_eq!(args.nonce, "n-1");
        assert_eq!(args.diff_base.as_deref(), Some("def456"));
        assert_eq!(args.task.as_deref(), Some("close the gap"));
    }

    #[test]
    fn a_mechanical_lane_dispatch_carries_no_model_axes() {
        // Tripwire: `--diff-base` absent is the *working-tree* contract, not a
        // parse miss. A parser that defaulted it to the subject would make
        // every member review judge a committed range and see an empty diff —
        // which is the shape that read as "nothing to review" in production.
        let args = parse(argv(&["verify.check", "--out", "/tmp/n-2-evidence", "--nonce", "n-2"])).unwrap();

        assert_eq!(args.command, "verify.check");
        assert_eq!(args.diff_base, None);
        assert_eq!(args.subject, None);
    }

    #[test]
    fn an_unknown_flag_is_ignored_rather_than_failing_the_run() {
        // Tripwire: the mock must keep running when the coordinator's argv
        // grows, or every new flag lands as a harness outage instead of a
        // scenario that still executes and records what arrived.
        let args = parse(argv(&["verify.check", "--out", "/tmp/e", "--nonce", "n", "--brand-new-flag"])).unwrap();

        assert_eq!(args.command, "verify.check");
    }

    #[test]
    fn an_argv_with_nowhere_to_write_is_refused() {
        assert_eq!(parse(argv(&["verify.check", "--nonce", "n"])), Err(ArgvError::MissingOut));
        assert_eq!(parse(argv(&["--out", "/tmp/e"])), Err(ArgvError::MissingCommand));
    }
}
