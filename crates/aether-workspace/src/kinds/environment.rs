//! An environment: a stored root tree plus what it declares (ADR-0237
//! decision 3).

use alloc::string::String;
use alloc::vec::Vec;

use aether_bloomery_kinds::{Ref, Tree};

use crate::kinds::order::{self, OrderError};
use crate::kinds::path::TreePath;
use crate::kinds::run::EnvVar;

const TOOL_NAME_MAX_BYTES: usize = 64;

/// Why [`ToolName::new`] or decode refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolNameError {
    /// The string was empty.
    Empty,
    /// The string was longer than 64 bytes.
    TooLong,
    /// A byte was outside `[A-Za-z0-9._+-]`.
    Char,
    /// The string began with `.` or `-`.
    Leading,
}

impl ToolNameError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Char => "char",
            Self::Leading => "leading",
        }
    }
}

/// Why [`Platform::new`] or decode refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformError {
    /// The triple had fewer than 3 or more than 4 `-`-separated segments.
    SegmentCount,
    /// A segment was empty.
    EmptySegment,
    /// A byte was outside `[a-z0-9_.]`.
    Char,
}

impl PlatformError {
    const fn reason(self) -> &'static str {
        match self {
            Self::SegmentCount => "segment-count",
            Self::EmptySegment => "empty-segment",
            Self::Char => "char",
        }
    }
}

/// Why [`RustToolchain::new`] or decode refused a toolchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustToolchainError {
    /// The channel was empty or held a byte outside `[A-Za-z0-9._-]`.
    Channel,
    /// A component was empty or held a byte outside `[A-Za-z0-9._-]`.
    Component,
    /// A target was empty or held a byte outside `[A-Za-z0-9._-]`.
    Target,
    /// Two components were equal.
    DuplicateComponent,
    /// Two targets were equal.
    DuplicateTarget,
    /// The components were not in ascending order (decode only).
    UnsortedComponents,
    /// The targets were not in ascending order (decode only).
    UnsortedTargets,
}

impl RustToolchainError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Component => "component",
            Self::Target => "target",
            Self::DuplicateComponent => "duplicate-component",
            Self::DuplicateTarget => "duplicate-target",
            Self::UnsortedComponents => "unsorted-components",
            Self::UnsortedTargets => "unsorted-targets",
        }
    }
}

/// Why [`Tools::new`] or decode refused a tool table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolsError {
    /// Two tools had the same name.
    Duplicate,
    /// The tools were not in ascending name order (decode only).
    Unsorted,
}

impl ToolsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Unsorted => "unsorted",
        }
    }

    const fn from_order(error: OrderError) -> Self {
        match error {
            OrderError::Duplicate => Self::Duplicate,
            OrderError::Unsorted => Self::Unsorted,
        }
    }
}

invariant_errors!(ToolNameError, PlatformError, RustToolchainError, ToolsError);

/// The name a step runs a tool by, resolved through [`Environment::tools`],
/// never a path: 1 to 64 bytes of `[A-Za-z0-9._+-]`, not starting with `.` or
/// `-`, so `g++` and `c++` are names and no name reads as a flag or a hidden
/// file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, aether_data::Storage)]
#[storage(validate)]
pub struct ToolName(String);

impl ToolName {
    /// Accept a tool name.
    ///
    /// # Errors
    ///
    /// [`ToolNameError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, ToolNameError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), ToolNameError> {
        if value.is_empty() {
            return Err(ToolNameError::Empty);
        }
        if value.len() > TOOL_NAME_MAX_BYTES {
            return Err(ToolNameError::TooLong);
        }
        if !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')) {
            return Err(ToolNameError::Char);
        }
        if value.starts_with(['.', '-']) {
            return Err(ToolNameError::Leading);
        }
        Ok(())
    }
}

/// A target triple, such as `x86_64-unknown-linux-gnu`: 3 or 4 `-`-separated
/// segments of `[a-z0-9_.]`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Platform(String);

impl Platform {
    /// Accept a target triple.
    ///
    /// # Errors
    ///
    /// [`PlatformError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, PlatformError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the triple as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), PlatformError> {
        if !(3..=4).contains(&value.split('-').count()) {
            return Err(PlatformError::SegmentCount);
        }
        value.split('-').try_for_each(|segment| {
            if segment.is_empty() {
                Err(PlatformError::EmptySegment)
            } else if segment.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.')) {
                Ok(())
            } else {
                Err(PlatformError::Char)
            }
        })
    }
}

/// The fields a [`RustToolchain`] validates as one value.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
struct ToolchainSpec {
    channel: String,
    components: Vec<String>,
    targets: Vec<String>,
}

/// The Rust toolchain an environment provides or a tree asks for: a
/// `channel` (`1.97.1`, `nightly-2026-09-01`) plus its extra `components`
/// and `targets`.
///
/// Every string is non-empty `[A-Za-z0-9._-]`. Components and targets are
/// sets, stored sorted and unique so equal toolchains have one encoding:
/// [`RustToolchain::new`] sorts them, and decode refuses an unsorted list.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct RustToolchain(ToolchainSpec);

impl RustToolchain {
    /// Accept a toolchain, sorting `components` and `targets`.
    ///
    /// # Errors
    ///
    /// [`RustToolchainError`] names which rule failed.
    pub fn new(
        channel: impl Into<String>,
        mut components: Vec<String>,
        mut targets: Vec<String>,
    ) -> Result<Self, RustToolchainError> {
        components.sort_unstable();
        targets.sort_unstable();
        let spec = ToolchainSpec { channel: channel.into(), components, targets };
        Self::check(&spec)?;
        Ok(Self(spec))
    }

    /// The release channel, as `rust-toolchain.toml` spells it.
    #[must_use]
    pub fn channel(&self) -> &str {
        &self.0.channel
    }

    /// The extra components, sorted.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.0.components
    }

    /// The extra targets, sorted.
    #[must_use]
    pub fn targets(&self) -> &[String] {
        &self.0.targets
    }

    fn check(spec: &ToolchainSpec) -> Result<(), RustToolchainError> {
        let word = |value: &String| {
            !value.is_empty()
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        };
        if !word(&spec.channel) {
            return Err(RustToolchainError::Channel);
        }
        if !spec.components.iter().all(word) {
            return Err(RustToolchainError::Component);
        }
        if !spec.targets.iter().all(word) {
            return Err(RustToolchainError::Target);
        }
        order::check(&spec.components, |component| component).map_err(|error| match error {
            OrderError::Duplicate => RustToolchainError::DuplicateComponent,
            OrderError::Unsorted => RustToolchainError::UnsortedComponents,
        })?;
        order::check(&spec.targets, |target| target).map_err(|error| match error {
            OrderError::Duplicate => RustToolchainError::DuplicateTarget,
            OrderError::Unsorted => RustToolchainError::UnsortedTargets,
        })
    }
}

/// What an environment declares it provides, compared with a tree's own
/// requirements before a run.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Provides {
    /// The Rust toolchain in the root, if any. A run over a tree with a
    /// `rust-toolchain.toml` that asks for another is refused.
    pub rust: Option<RustToolchain>,
}

/// One entry of [`Tools`]: a tool name and the executable it resolves to.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Tool {
    /// The name a step runs it by.
    pub name: ToolName,
    /// Its path inside the environment root, which must hold a
    /// `Node::Executable` there.
    pub path: TreePath,
}

/// The environment's tool table: each [`ToolName`] maps to one path.
///
/// A name maps once. The table is stored in ascending name order so equal
/// tables have one encoding: [`Tools::new`] sorts, and decode refuses an
/// unsorted table.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Tools(Vec<Tool>);

impl Tools {
    /// Accept a tool table, sorting it by name.
    ///
    /// # Errors
    ///
    /// [`ToolsError::Duplicate`] when two tools share a name.
    pub fn new(mut tools: Vec<Tool>) -> Result<Self, ToolsError> {
        tools.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Self::check(&tools)?;
        Ok(Self(tools))
    }

    /// Every tool, in ascending name order.
    #[must_use]
    pub fn as_slice(&self) -> &[Tool] {
        &self.0
    }

    fn check(tools: &[Tool]) -> Result<(), ToolsError> {
        order::check(tools, |tool| &tool.name).map_err(ToolsError::from_order)
    }
}

/// A stored environment: the only files a run sees, and what they declare.
///
/// A [`crate::Run`] cites one as `Ref<Environment>`. The kernel is never part
/// of an environment.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "aether.workspace.environment")]
pub struct Environment {
    /// The whole visible root filesystem.
    pub root: Ref<Tree>,
    /// The target triple the root's binaries run on.
    pub platform: Platform,
    /// What the root provides, such as a Rust toolchain.
    pub provides: Provides,
    /// Tool names and the executables they resolve to inside `root`.
    pub tools: Tools,
    /// The base environment (`PATH`, `LANG`, ...), fixed per environment;
    /// each step's own variables merge over it.
    pub env: Vec<EnvVar>,
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_bloomery_kinds::{Digest, Ref};
    use aether_data::wire::{Error as WireError, decode_from_slice, encode_to_vec};
    use aether_data::{Storage, StorageData};

    use super::{
        Environment, Platform, PlatformError, Provides, RustToolchain, RustToolchainError, TOOL_NAME_MAX_BYTES, Tool,
        ToolName, ToolNameError, ToolchainSpec, Tools, ToolsError,
    };
    use crate::kinds::path::TreePath;
    use crate::kinds::test_support::assert_rule;

    fn tool(name: &str) -> Tool {
        Tool { name: ToolName::new(name).expect("tool name"), path: TreePath::new("usr/bin/tool").expect("tool path") }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|&value| String::from(value)).collect()
    }

    fn spec(channel: &str, components: &[&str], targets: &[&str]) -> ToolchainSpec {
        ToolchainSpec { channel: String::from(channel), components: strings(components), targets: strings(targets) }
    }

    #[test]
    fn tool_name_rules_refuse_and_accept_their_neighbours() {
        let too_long = "a".repeat(TOOL_NAME_MAX_BYTES + 1);
        let max_len = "a".repeat(TOOL_NAME_MAX_BYTES);
        let cases = [
            ("", ToolNameError::Empty, "a"),
            (too_long.as_str(), ToolNameError::TooLong, max_len.as_str()),
            ("usr/bin/cc", ToolNameError::Char, "g++"),
            ("c c", ToolNameError::Char, "c_c"),
            (".cargo", ToolNameError::Leading, "cargo.real"),
            ("-rf", ToolNameError::Leading, "rf-"),
        ];
        for (reject, error, accept) in cases {
            assert_rule(ToolName::new, String::from(reject), error, String::from(accept));
        }
    }

    #[test]
    fn platform_rules_refuse_and_accept_their_neighbours() {
        let cases = [
            ("x86_64-linux", PlatformError::SegmentCount, "x86_64-unknown-linux"),
            ("x86_64-unknown-linux-gnu-extra", PlatformError::SegmentCount, "x86_64-unknown-linux-gnu"),
            ("x86_64--linux", PlatformError::EmptySegment, "x86_64-pc-linux"),
            ("x86_64-Apple-darwin", PlatformError::Char, "x86_64-apple-darwin"),
        ];
        for (reject, error, accept) in cases {
            assert_rule(Platform::new, String::from(reject), error, String::from(accept));
        }
    }

    #[test]
    fn toolchain_rules_refuse_and_accept_their_neighbours() {
        let new = |spec: ToolchainSpec| RustToolchain::new(spec.channel, spec.components, spec.targets);
        let cases = [
            (spec("", &[], &[]), RustToolchainError::Channel, spec("1.97.1", &[], &[])),
            (spec("1.97 1", &[], &[]), RustToolchainError::Channel, spec("nightly-2026-09-01", &[], &[])),
            (spec("1.97.1", &[""], &[]), RustToolchainError::Component, spec("1.97.1", &["clippy"], &[])),
            (
                spec("1.97.1", &[], &["wasm32/wasip1"]),
                RustToolchainError::Target,
                spec("1.97.1", &[], &["wasm32-wasip1"]),
            ),
            (
                spec("1.97.1", &["clippy", "clippy"], &[]),
                RustToolchainError::DuplicateComponent,
                spec("1.97.1", &["clippy", "rustfmt"], &[]),
            ),
            (
                spec("1.97.1", &[], &["wasm32-wasip1", "wasm32-wasip1"]),
                RustToolchainError::DuplicateTarget,
                spec("1.97.1", &[], &["wasm32-unknown-unknown", "wasm32-wasip1"]),
            ),
        ];
        for (reject, error, accept) in cases {
            assert_rule(new, reject, error, accept);
        }
    }

    #[test]
    fn sets_are_sorted_by_new_and_refused_unsorted_on_decode() {
        // Catches a constructor that stops sorting, which would give equal
        // sets two encodings and two digests, and a decode that stops checking
        // order, which would accept the second encoding.
        let toolchain = RustToolchain::new("1.97.1", strings(&["rustfmt", "clippy"]), strings(&["b", "a"]))
            .expect("unsorted input is sorted");
        assert_eq!(
            (toolchain.components(), toolchain.targets()),
            (&*strings(&["clippy", "rustfmt"]), &*strings(&["a", "b"]))
        );
        let tools = Tools::new(vec![tool("rustc"), tool("cargo")]).expect("unsorted input is sorted");
        assert_eq!(tools.as_slice(), [tool("cargo"), tool("rustc")]);

        let decode = |spec: ToolchainSpec| decode_from_slice::<RustToolchain>(&encode_to_vec(&spec).expect("encode"));
        let refused = |reason: &str| Err(WireError::Message(String::from(reason)));
        assert_eq!(decode(spec("1.97.1", &["rustfmt", "clippy"], &[])), refused("unsorted-components"));
        assert_eq!(decode(spec("1.97.1", &[], &["b", "a"])), refused("unsorted-targets"));
    }

    #[test]
    fn a_stored_environment_refuses_a_duplicate_or_unsorted_tool_table() {
        // Catches a dropped `#[storage(validate)]` on `Tools`, which would let
        // a name map twice through the journal.
        assert_eq!(Tools::new(vec![tool("cc"), tool("cc")]), Err(ToolsError::Duplicate));

        let environment = |tools: Vec<Tool>| Environment {
            root: Ref::from_digest(Digest::from_bytes([7; 32])),
            platform: Platform::new("x86_64-unknown-linux-gnu").expect("platform"),
            provides: Provides { rust: None },
            tools: Tools(tools),
            env: Vec::new(),
        };
        let round_trip = |tools: Vec<Tool>| {
            let bytes = Environment::encode_storage(&StorageData::from_value(environment(tools))).expect("encode");
            Environment::decode_storage(&bytes).map(|data| data.value)
        };

        let sorted = vec![tool("cargo"), tool("rustc")];
        assert_eq!(round_trip(sorted.clone()).ok(), Some(environment(sorted)));
        assert!(round_trip(vec![tool("cc"), tool("cc")]).is_err(), "a duplicate name refuses");
        assert!(round_trip(vec![tool("rustc"), tool("cargo")]).is_err(), "an unsorted table refuses");
    }
}
