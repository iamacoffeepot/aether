//! The run's knobs, read from the environment: `aether-perf-compare` spawns a
//! trial binary with no arguments and passes knobs only through `--base-env` /
//! `--cand-env`.

use std::env::{self, VarError};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use aether_bloomery_muse::{ToolCalls, TurnItems};

/// How many sessions run at once.
const SESSIONS: &str = "AETHER_PERF_BLOOMERY_SESSIONS";
/// How many turns each session makes, the last one completing it.
const TURNS: &str = "AETHER_PERF_BLOOMERY_TURNS";
/// How many tool calls each turn but the last asks for.
const CALLS: &str = "AETHER_PERF_BLOOMERY_CALLS";
/// Which tools the calls rotate over.
const TOOLS: &str = "AETHER_PERF_BLOOMERY_TOOLS";
/// The tree each session opens on.
const TREE: &str = "AETHER_PERF_BLOOMERY_TREE";

/// A tool the stub vendor asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// `tree.write` of a new file under `bench/`.
    Write,
    /// `tree.list` of the root.
    List,
    /// `tree.read` of a seeded file.
    Read,
    /// `tree.grep` for the seeded target.
    Grep,
}

impl Tool {
    /// The knob spelling, which is also the tool's part of the shape.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::List => "list",
            Self::Read => "read",
            Self::Grep => "grep",
        }
    }

    /// The function name a turn offers the tool under.
    pub const fn function(self) -> &'static str {
        match self {
            Self::Write => "tree-write",
            Self::List => "tree-list",
            Self::Read => "tree-read",
            Self::Grep => "tree-grep",
        }
    }

    fn parse(label: &str) -> Option<Self> {
        [Self::Write, Self::List, Self::Read, Self::Grep].into_iter().find(|tool| tool.label() == label)
    }
}

/// The tree each session opens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeSpec {
    /// `files` generated files of `bytes` bytes each, 16 to a directory.
    Synthetic { files: usize, bytes: usize },
    /// A host directory, staged node by node.
    Directory(PathBuf),
}

/// A knob whose value is malformed or past a bound, named with the bound.
#[derive(Debug)]
pub struct KnobError {
    knob: &'static str,
    value: String,
    bound: String,
}

impl fmt::Display for KnobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.knob, self.value, self.bound)
    }
}

/// Every knob of one run.
#[derive(Debug, Clone)]
pub struct Knobs {
    pub sessions: u32,
    pub turns: u32,
    pub calls: usize,
    pub tools: Vec<Tool>,
    pub tree: TreeSpec,
}

impl Knobs {
    /// Read every knob, each defaulting when unset or empty, and check the
    /// shape against the Muse input limits.
    pub fn from_env() -> Result<Self, KnobError> {
        let sessions = number(SESSIONS, 1)?;
        let turns = number(TURNS, 8)?;
        let calls = number(CALLS, 4)?;
        let tools = read(TOOLS)?.map_or_else(|| Ok(vec![Tool::Write]), |value| tools(&value))?;
        let tree =
            read(TREE)?.map_or_else(|| Ok(TreeSpec::Synthetic { files: 64, bytes: 4096 }), |value| tree(&value))?;

        at_least_one(SESSIONS, sessions)?;
        at_least_one(TURNS, turns)?;
        if calls == 0 || calls > ToolCalls::MAX_CALLS {
            return Err(refuse(CALLS, calls, format!("a turn asks for 1 to {} calls", ToolCalls::MAX_CALLS)));
        }
        let items = rested_items(turns, calls);
        if items.is_none_or(|items| items > TurnItems::MAX_ITEMS) {
            let bound = format!(
                "a session at rest holds 2 + (turns - 1) * 2 * calls items, at most {} (turns {turns}, calls {calls})",
                TurnItems::MAX_ITEMS
            );
            return Err(refuse(TURNS, turns, bound));
        }
        Ok(Self { sessions, turns, calls, tools, tree })
    }

    /// The workload shape a report cell names: `t{turns}-c{calls}-{tools}-{tree}`.
    pub fn shape(&self) -> String {
        let tools = self.tools.iter().map(|tool| tool.label()).collect::<Vec<_>>().join("+");
        let tree = match &self.tree {
            TreeSpec::Synthetic { files, bytes } => format!("{files}x{bytes}"),
            TreeSpec::Directory(path) => {
                format!("dir-{}", path.file_name().map_or_else(|| "root".into(), |name| name.to_string_lossy()))
            }
        };
        format!("t{}-c{}-{tools}-{tree}", self.turns, self.calls)
    }
}

/// The items a completed session holds at rest: the user message, each called
/// turn's calls and outputs, and the final answer.
fn rested_items(turns: u32, calls: usize) -> Option<usize> {
    let called = usize::try_from(turns - 1).ok()?;
    called.checked_mul(calls)?.checked_mul(2)?.checked_add(2)
}

// Dev/perf tooling: this benchmark takes its run parameters from env, as
// perf-trial does, because perf-compare passes knobs only through env — not a
// capability, no config layer in scope.
#[allow(clippy::disallowed_methods)]
fn read(knob: &'static str) -> Result<Option<String>, KnobError> {
    match env::var(knob) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(value)) => Err(refuse(knob, value.to_string_lossy(), "not Unicode".into())),
    }
}

/// A non-negative integer knob, or `default` when unset.
fn number<N: FromStr>(knob: &'static str, default: N) -> Result<N, KnobError> {
    read(knob)?.map_or_else(
        || Ok(default),
        |value| value.parse().map_err(|_| refuse(knob, &value, "not a non-negative integer".into())),
    )
}

fn at_least_one(knob: &'static str, value: u32) -> Result<(), KnobError> {
    if value == 0 {
        Err(refuse(knob, value, "at least 1".into()))
    } else {
        Ok(())
    }
}

/// A comma list of tool labels, at least one.
fn tools(value: &str) -> Result<Vec<Tool>, KnobError> {
    let tools = value
        .split(',')
        .map(|label| Tool::parse(label.trim()))
        .collect::<Option<Vec<_>>>()
        .filter(|tools| !tools.is_empty())
        .ok_or_else(|| refuse(TOOLS, value, "a comma list of write, list, read, and grep".into()))?;
    Ok(tools)
}

/// `<files>x<bytes>`, each at least 1, or `dir:<path>`.
fn tree(value: &str) -> Result<TreeSpec, KnobError> {
    if let Some(path) = value.strip_prefix("dir:") {
        return if path.is_empty() {
            Err(refuse(TREE, value, "dir: names a host directory".into()))
        } else {
            Ok(TreeSpec::Directory(Path::new(path).to_path_buf()))
        };
    }
    value
        .split_once('x')
        .and_then(|(files, bytes)| Some((files.parse().ok()?, bytes.parse().ok()?)))
        .filter(|&(files, bytes)| files > 0 && bytes > 0)
        .map(|(files, bytes)| TreeSpec::Synthetic { files, bytes })
        .ok_or_else(|| refuse(TREE, value, "<files>x<bytes>, each at least 1, or dir:<path>".into()))
}

fn refuse(knob: &'static str, value: impl fmt::Display, bound: String) -> KnobError {
    KnobError { knob, value: value.to_string(), bound }
}
