//! The run request: steps over a stored tree in a stored environment
//! (ADR-0237 decision 2).

use alloc::string::String;
use alloc::vec::Vec;

use aether_bloomery_kinds::{OpaqueBytes, Ref, Tree};

use crate::kinds::environment::{Environment, ToolName};
use crate::kinds::order::{self, OrderError};
use crate::kinds::path::{TreePath, covers};

/// Most steps one [`Run`] may carry.
pub const MAX_STEPS: usize = 64;

const ENV_KEY_MAX_BYTES: usize = 256;
const ENV_VALUE_MAX_BYTES: usize = 32 * 1024;

/// Why [`EnvVar::new`] or decode refused a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvVarError {
    /// The key was empty.
    KeyEmpty,
    /// The key was longer than 256 bytes.
    KeyTooLong,
    /// The key broke `[A-Za-z_][A-Za-z0-9_]*`.
    KeyChar,
    /// The value was longer than 32 KiB.
    ValueTooLong,
    /// The value contained NUL, which no process environment can hold.
    ValueNul,
}

impl EnvVarError {
    const fn reason(self) -> &'static str {
        match self {
            Self::KeyEmpty => "key-empty",
            Self::KeyTooLong => "key-too-long",
            Self::KeyChar => "key-char",
            Self::ValueTooLong => "value-too-long",
            Self::ValueNul => "value-nul",
        }
    }
}

/// Why [`Mounts::new`] or decode refused a mount list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountsError {
    /// A mount was at `work`, under it, or above it, where the run's own tree
    /// is written.
    OverlapsWork,
    /// Two mounts were at the same path, or one was under the other.
    Overlap,
}

impl MountsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::OverlapsWork => "overlaps-work",
            Self::Overlap => "overlap",
        }
    }
}

/// Why [`Scratch::new`] or decode refused a scratch list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScratchError {
    /// Two paths were equal.
    Duplicate,
    /// The paths were not in ascending order (decode only).
    Unsorted,
}

impl ScratchError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Unsorted => "unsorted",
        }
    }
}

/// Why [`Steps::new`] or decode refused a step list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepsError {
    /// The list was empty.
    Empty,
    /// The list held more than [`MAX_STEPS`] steps.
    TooMany,
}

impl StepsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooMany => "too-many",
        }
    }
}

invariant_errors!(EnvVarError, MountsError, ScratchError, StepsError);

/// The two halves an [`EnvVar`] validates as one value.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
struct EnvEntry {
    key: String,
    value: String,
}

/// One environment variable: a key of `[A-Za-z_][A-Za-z0-9_]*` of at most
/// 256 bytes, and a value of at most 32 KiB with no NUL.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct EnvVar(EnvEntry);

impl EnvVar {
    /// Accept one variable.
    ///
    /// # Errors
    ///
    /// [`EnvVarError`] names which rule failed.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Result<Self, EnvVarError> {
        let entry = EnvEntry { key: key.into(), value: value.into() };
        Self::check(&entry)?;
        Ok(Self(entry))
    }

    /// The variable's name.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.0.key
    }

    /// The variable's value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.0.value
    }

    fn check(entry: &EnvEntry) -> Result<(), EnvVarError> {
        let Some(first) = entry.key.bytes().next() else {
            return Err(EnvVarError::KeyEmpty);
        };
        if entry.key.len() > ENV_KEY_MAX_BYTES {
            return Err(EnvVarError::KeyTooLong);
        }
        let rest_valid = entry.key.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if first.is_ascii_digit() || !rest_valid {
            return Err(EnvVarError::KeyChar);
        }
        if entry.value.len() > ENV_VALUE_MAX_BYTES {
            return Err(EnvVarError::ValueTooLong);
        }
        if entry.value.contains('\0') {
            return Err(EnvVarError::ValueNul);
        }
        Ok(())
    }
}

/// Whether a run may reach the network. A capability grant, not a resource:
/// `Off` unless the program must fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Schema)]
pub enum Network {
    Off,
    On,
}

/// An extra read-only tree written out at `at` before the first step.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Mount {
    /// Where the tree appears, relative to the root.
    pub at: TreePath,
    /// The tree to write there.
    pub tree: Ref<Tree>,
}

/// A run's extra mounts. None is at [`Mounts::WORK`], under it, or above it,
/// and no two are at the same path or one under the other, so every path a
/// run sees comes from exactly one tree.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Mounts(Vec<Mount>);

impl Mounts {
    /// Where the run's own tree is written, relative to the root.
    pub const WORK: &'static str = "work";

    /// Accept a mount list.
    ///
    /// # Errors
    ///
    /// [`MountsError`] names which rule failed.
    pub fn new(mounts: Vec<Mount>) -> Result<Self, MountsError> {
        Self::check(&mounts)?;
        Ok(Self(mounts))
    }

    /// Every mount, in request order.
    #[must_use]
    pub fn as_slice(&self) -> &[Mount] {
        &self.0
    }

    fn check(mounts: &[Mount]) -> Result<(), MountsError> {
        let overlaps_work = |at: &str| covers(at, Self::WORK) || covers(Self::WORK, at);
        if mounts.iter().any(|mount| overlaps_work(mount.at.as_str())) {
            return Err(MountsError::OverlapsWork);
        }
        // In segment order a path sorts directly before the paths under it,
        // so comparing neighbours finds every nested or equal pair.
        let mut paths: Vec<&TreePath> = mounts.iter().map(|mount| &mount.at).collect();
        paths.sort_unstable_by(|left, right| left.as_str().split('/').cmp(right.as_str().split('/')));
        if paths.windows(2).any(|pair| covers(pair[0].as_str(), pair[1].as_str())) {
            return Err(MountsError::Overlap);
        }
        Ok(())
    }
}

/// Paths under `/work` left out of the output tree, such as `target`.
///
/// A set, stored sorted and unique so equal sets have one encoding:
/// [`Scratch::new`] sorts, and decode refuses an unsorted list.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Scratch(Vec<TreePath>);

impl Scratch {
    /// Accept a scratch list, sorting it.
    ///
    /// # Errors
    ///
    /// [`ScratchError::Duplicate`] when two paths are equal.
    pub fn new(mut paths: Vec<TreePath>) -> Result<Self, ScratchError> {
        paths.sort_unstable();
        Self::check(&paths)?;
        Ok(Self(paths))
    }

    /// Every path, in ascending order.
    #[must_use]
    pub fn as_slice(&self) -> &[TreePath] {
        &self.0
    }

    fn check(paths: &[TreePath]) -> Result<(), ScratchError> {
        order::check(paths, |path| path).map_err(|error| match error {
            OrderError::Duplicate => ScratchError::Duplicate,
            OrderError::Unsorted => ScratchError::Unsorted,
        })
    }
}

/// One process: a tool from the environment's table, its argv, and its
/// variables. There is never a shell.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Step {
    /// Resolved through [`Environment::tools`], never a path.
    pub tool: ToolName,
    /// The arguments after the tool, passed as argv.
    pub args: Vec<String>,
    /// Variables merged over the environment's base `env`.
    pub env: Vec<EnvVar>,
    /// Bytes written to the process's stdin, if any.
    pub stdin: Option<Ref<OpaqueBytes>>,
}

/// A run's steps, in order: 1 to [`MAX_STEPS`].
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Steps(Vec<Step>);

impl Steps {
    /// Accept a step list.
    ///
    /// # Errors
    ///
    /// [`StepsError`] names which rule failed.
    pub fn new(steps: Vec<Step>) -> Result<Self, StepsError> {
        Self::check(&steps)?;
        Ok(Self(steps))
    }

    /// Every step, in run order.
    #[must_use]
    pub fn as_slice(&self) -> &[Step] {
        &self.0
    }

    fn check(steps: &[Step]) -> Result<(), StepsError> {
        if steps.is_empty() {
            Err(StepsError::Empty)
        } else if steps.len() > MAX_STEPS {
            Err(StepsError::TooMany)
        } else {
            Ok(())
        }
    }
}

/// Run `steps` over `tree` in `environment`. Answered with one
/// [`crate::RunResult`]; nothing is held open between runs.
///
/// The request names no resource amounts: cores, memory, and the deadline are
/// the executor's to choose.
#[aether_data::kind(name = "aether.workspace.run", eq, no_serde)]
pub struct Run {
    /// Written out at `/work`.
    pub tree: Ref<Tree>,
    /// The whole visible root filesystem and its tool table.
    pub environment: Ref<Environment>,
    /// Extra read-only trees, such as vendored crates.
    pub mounts: Mounts,
    /// The processes to run, in order.
    pub steps: Steps,
    /// Paths under `/work` excluded from the output tree.
    pub scratch: Scratch,
    /// Whether the run may reach the network.
    pub network: Network,
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_bloomery_kinds::{Digest, Ref, Tree};

    use super::{
        ENV_KEY_MAX_BYTES, ENV_VALUE_MAX_BYTES, EnvEntry, EnvVar, EnvVarError, MAX_STEPS, Mount, Mounts, MountsError,
        Scratch, ScratchError, Step, Steps, StepsError,
    };
    use crate::kinds::environment::ToolName;
    use crate::kinds::path::TreePath;
    use crate::kinds::test_support::assert_rule;

    fn mount(at: &str) -> Mount {
        Mount {
            at: TreePath::new(at).expect("mount path"),
            tree: Ref::<Tree>::from_digest(Digest::from_bytes([1; 32])),
        }
    }

    fn mounts(paths: &[&str]) -> Vec<Mount> {
        paths.iter().map(|&path| mount(path)).collect()
    }

    fn step() -> Step {
        Step { tool: ToolName::new("cargo").expect("tool"), args: Vec::new(), env: Vec::new(), stdin: None }
    }

    #[test]
    fn env_var_rules_refuse_and_accept_their_neighbours() {
        let entry = |key: &str, value: &str| EnvEntry { key: String::from(key), value: String::from(value) };
        let new = |entry: EnvEntry| EnvVar::new(entry.key, entry.value);
        let long_key = "K".repeat(ENV_KEY_MAX_BYTES + 1);
        let max_key = "K".repeat(ENV_KEY_MAX_BYTES);
        let long_value = "v".repeat(ENV_VALUE_MAX_BYTES + 1);
        let max_value = "v".repeat(ENV_VALUE_MAX_BYTES);
        let cases = [
            (entry("", "v"), EnvVarError::KeyEmpty, entry("_", "v")),
            (entry(&long_key, "v"), EnvVarError::KeyTooLong, entry(&max_key, "v")),
            (entry("1PATH", "v"), EnvVarError::KeyChar, entry("PATH1", "v")),
            (entry("CARGO-HOME", "v"), EnvVarError::KeyChar, entry("CARGO_HOME", "v")),
            (entry("K", &long_value), EnvVarError::ValueTooLong, entry("K", &max_value)),
            (entry("K", "a\0b"), EnvVarError::ValueNul, entry("K", "a=b")),
        ];
        for (reject, error, accept) in cases {
            assert_rule(new, reject, error, accept);
        }
    }

    #[test]
    fn mounts_refuse_work_and_nesting_and_accept_their_neighbours() {
        // Catches an overlap test that compares string prefixes instead of
        // segments (refusing `workspace` beside `work`), or that compares only
        // unsorted neighbours (missing `a` and `a/b` with `a-b` between them).
        let cases = [
            (mounts(&["work"]), MountsError::OverlapsWork, mounts(&["workspace"])),
            (mounts(&["work/vendor"]), MountsError::OverlapsWork, mounts(&["vendor/work"])),
            (mounts(&["opt", "opt"]), MountsError::Overlap, mounts(&["opt", "opt2"])),
            (mounts(&["opt/a/b", "opt/a"]), MountsError::Overlap, mounts(&["opt/a/b", "opt/ab"])),
            (mounts(&["a/b", "a-b", "a"]), MountsError::Overlap, mounts(&["a/b", "a-b", "b"])),
        ];
        for (reject, error, accept) in cases {
            assert_rule(Mounts::new, reject, error, accept);
        }
    }

    #[test]
    fn scratch_refuses_duplicates_and_accepts_distinct_paths() {
        let paths = |values: &[&str]| -> Vec<TreePath> {
            values.iter().map(|&value| TreePath::new(value).expect("scratch path")).collect()
        };
        assert_rule(Scratch::new, paths(&["target", "target"]), ScratchError::Duplicate, paths(&["target", "tmp"]));
        assert_eq!(
            Scratch::new(paths(&["tmp", "target"])).map(|scratch| scratch.as_slice().to_vec()),
            Ok(paths(&["target", "tmp"]))
        );
    }

    #[test]
    fn steps_count_bounds_are_inclusive() {
        assert_rule(Steps::new, Vec::new(), StepsError::Empty, vec![step()]);
        assert_rule(Steps::new, vec![step(); MAX_STEPS + 1], StepsError::TooMany, vec![step(); MAX_STEPS]);
    }
}
