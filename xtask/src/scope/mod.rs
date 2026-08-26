//! `cargo xtask scope set` — the transport a `scope.fill` model writes through.
//!
//! A problem statement or a plan step is multi-paragraph prose containing
//! quotes, backticks and newlines. A setter whose value arrives as a
//! shell-quoted argv scalar truncates at the first such character, and the
//! field looks filled. The value arrives by file (`--value-file`, `-` for
//! stdin) and is appended to a per-run call log the lane replays.

mod log;

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

pub use log::{append, load, parse_field, replay, winning_texts};

/// `cargo xtask scope` — currently the setter, which is the only subcommand
/// the scoping lane needs.
#[derive(Args, Debug)]
pub struct ScopeArgs {
    #[command(subcommand)]
    command: ScopeCommand,
}

#[derive(Subcommand, Debug)]
enum ScopeCommand {
    /// Append one field write to a `scope.fill` run's call log.
    Set(SetArgs),
}

#[derive(Args, Debug)]
struct SetArgs {
    /// Authored field kind: problem, evidence, success, approach,
    /// rejected-option, plan-step, acceptance, declared-surface, edge, or
    /// routing-hint. Derived kinds (`inverse-search`, `implements`) are refused.
    field: String,
    /// The transform run directory that owns the call log.
    #[arg(long)]
    run: PathBuf,
    /// Path to the field's value. `-` reads stdin so a multi-paragraph value
    /// never has to survive shell quoting.
    #[arg(long)]
    value_file: PathBuf,
}

/// Dispatch `cargo xtask scope`.
///
/// # Errors
/// The field name is refused, the value file cannot be read, or the log cannot
/// be appended.
pub fn run(args: &ScopeArgs) -> Result<()> {
    match &args.command {
        ScopeCommand::Set(args) => set(&args.field, &args.run, &args.value_file),
    }
}

/// Append one authored field write to `run`'s call log.
///
/// # Errors
/// The field name is refused, the value file cannot be read, or the log cannot
/// be appended.
pub fn set(field: &str, run: &Path, value_file: &Path) -> Result<()> {
    let kind = parse_field(field)?;
    let value = read_value(value_file)?;
    append(run, kind, value)
}

fn read_value(path: &Path) -> Result<String> {
    let raw = if path.as_os_str() == "-" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).context("read field value from stdin")?;
        buf
    } else {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    };
    // Interior newlines are content ("a value with newlines survives"), but the
    // trailing newline is the file format's, not the field's: left in place it
    // rides into every entry, and a single-path declared-surface value then
    // fails the surface grammar's character check on a glob that is otherwise
    // exactly right.
    Ok(raw.trim_end().to_owned())
}
