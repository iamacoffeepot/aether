//! What a proof tool takes: the arguments the model writes for each proof,
//! and the session values every proof binds, the environment, the vendor
//! tree, the cargo config, and the test env.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::Tree;
use aether_bloomery_workspace::{EnvVar, Environment};
use aether_data::{Invariant, Ref};

use super::config;

/// The arguments of `proof.clippy`: none. The proof always runs over the
/// whole workspace in the session's tree, so the model writes `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.clippy.args")]
pub struct ClippyArgs;

/// The arguments of `proof.test`: an optional scope narrowing the run by
/// target and filter. The default is the whole workspace, so the model
/// writes `{}` for today's whole run.
#[derive(Debug, Clone, Default, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.test.args")]
pub struct TestArgs {
    scope: TestScope,
}

impl TestArgs {
    /// Accept test arguments over `scope`.
    #[must_use]
    pub const fn new(scope: TestScope) -> Self {
        Self { scope }
    }

    /// The scope the run builds and runs.
    #[must_use]
    pub const fn scope(&self) -> &TestScope {
        &self.scope
    }
}

/// Most bytes one [`ScopeEntry`] may hold.
pub const MAX_SCOPE_ENTRY_BYTES: usize = 256;

/// Most entries one [`ScopeEntries`] axis may hold.
pub const MAX_SCOPE_ENTRIES: usize = 32;

/// Why [`ScopeEntry::new`] or decode refused a scope entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeEntryError {
    /// The entry was empty.
    Empty,
    /// The entry was longer than [`MAX_SCOPE_ENTRY_BYTES`] bytes.
    TooLong,
    /// The first byte was not `[A-Za-z0-9_]`.
    BadStart,
    /// A later byte was not `[A-Za-z0-9_.:/-]`.
    BadChar,
}

impl Invariant for ScopeEntryError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::BadStart => "bad-start",
            Self::BadChar => "bad-char",
        }
    }
}

impl fmt::Display for ScopeEntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Invariant::reason(self))
    }
}

impl Error for ScopeEntryError {}

/// One scope entry: a cargo test target name or a libtest filter, 1 to
/// [`MAX_SCOPE_ENTRY_BYTES`] bytes matching `[A-Za-z0-9_][A-Za-z0-9_.:/-]*`.
/// The leading-dash refusal keeps no entry parsing as a cargo flag.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ScopeEntry(String);

impl ScopeEntry {
    /// Accept a scope entry.
    ///
    /// # Errors
    ///
    /// [`ScopeEntryError`] names which rule failed.
    pub fn new(entry: impl Into<String>) -> Result<Self, ScopeEntryError> {
        let entry = entry.into();
        Self::check(&entry)?;
        Ok(Self(entry))
    }

    /// Borrow the entry.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(entry: &str) -> Result<(), ScopeEntryError> {
        let Some((&first, rest)) = entry.as_bytes().split_first() else {
            return Err(ScopeEntryError::Empty);
        };
        if entry.len() > MAX_SCOPE_ENTRY_BYTES {
            return Err(ScopeEntryError::TooLong);
        }
        let start_ok = first.is_ascii_alphanumeric() || first == b'_';
        if !start_ok {
            return Err(ScopeEntryError::BadStart);
        }
        let rest_valid =
            rest.iter().all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_' || b".:/-".contains(&byte));
        if !rest_valid {
            return Err(ScopeEntryError::BadChar);
        }
        Ok(())
    }
}

/// Why [`ScopeEntries::new`] or decode refused a scope list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeEntriesError {
    /// The list held more than [`MAX_SCOPE_ENTRIES`] entries.
    TooMany,
}

impl Invariant for ScopeEntriesError {
    fn reason(&self) -> &'static str {
        match self {
            Self::TooMany => "too-many",
        }
    }
}

impl fmt::Display for ScopeEntriesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Invariant::reason(self))
    }
}

impl Error for ScopeEntriesError {}

/// One scope axis: at most [`MAX_SCOPE_ENTRIES`] entries, in the order
/// supplied. Empty selects everything on that axis.
#[derive(Debug, Clone, Default, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ScopeEntries(Vec<ScopeEntry>);

impl ScopeEntries {
    /// Accept a scope list.
    ///
    /// # Errors
    ///
    /// [`ScopeEntriesError`] names which rule failed.
    pub fn new(entries: Vec<ScopeEntry>) -> Result<Self, ScopeEntriesError> {
        Self::check(&entries)?;
        Ok(Self(entries))
    }

    /// Every entry, in the order supplied.
    #[must_use]
    pub fn as_slice(&self) -> &[ScopeEntry] {
        &self.0
    }

    fn check(entries: &[ScopeEntry]) -> Result<(), ScopeEntriesError> {
        if entries.len() > MAX_SCOPE_ENTRIES {
            return Err(ScopeEntriesError::TooMany);
        }
        Ok(())
    }
}

/// What a scoped `proof.test` builds and runs: the `--workspace` package
/// set narrowed by integration-test target and by libtest filter. Targets
/// each become a `--test <name>` pair, selecting integration test targets
/// only; filters pass after a `--` separator cargo forwards to test
/// binaries untouched. Unit tests in `src/` select by filter only: test
/// names are `module::test` paths with no crate qualifier, so a filter
/// matching that crate's module paths runs its unit tests while cargo
/// builds the whole workspace test graph warm off the shared layer. For
/// unit tests the scope saves run time only, never build narrowing.
#[derive(Debug, Clone, Default, PartialEq, Eq, aether_data::Storage)]
pub struct TestScope {
    targets: ScopeEntries,
    filters: ScopeEntries,
}

impl TestScope {
    /// Accept a scope over `targets` and `filters`.
    #[must_use]
    pub const fn new(targets: ScopeEntries, filters: ScopeEntries) -> Self {
        Self { targets, filters }
    }

    /// Whether the scope selects the whole workspace: both axes empty.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        let no_targets = self.targets.as_slice().is_empty();
        let no_filters = self.filters.as_slice().is_empty();
        no_targets && no_filters
    }

    /// The integration-test targets, in the order supplied.
    #[must_use]
    pub fn targets(&self) -> &[ScopeEntry] {
        self.targets.as_slice()
    }

    /// The libtest filters, in the order supplied.
    #[must_use]
    pub fn filters(&self) -> &[ScopeEntry] {
        self.filters.as_slice()
    }
}

/// Most variables one [`TestEnv`] may hold.
pub const MAX_TEST_ENV: usize = 32;

/// Why [`TestEnv::new`] or decode refused a test env list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEnvError {
    /// The list held more than [`MAX_TEST_ENV`] entries.
    TooMany,
    /// Two entries shared one key.
    DuplicateKey,
}

impl Invariant for TestEnvError {
    fn reason(&self) -> &'static str {
        match self {
            Self::TooMany => "too-many",
            Self::DuplicateKey => "duplicate-key",
        }
    }
}

impl fmt::Display for TestEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Invariant::reason(self))
    }
}

impl Error for TestEnvError {}

/// The session-supplied variables a test proof hands cargo: at most
/// [`MAX_TEST_ENV`] entries with no repeated key. Empty is allowed.
///
/// Whoever opens the session supplies it, and it reaches the test step only,
/// never fmt's or clippy's, so clippy's run key and warm layer are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct TestEnv(Vec<EnvVar>);

impl TestEnv {
    /// Accept a test env list.
    ///
    /// # Errors
    ///
    /// [`TestEnvError`] names which rule failed.
    pub fn new(vars: Vec<EnvVar>) -> Result<Self, TestEnvError> {
        Self::check(&vars)?;
        Ok(Self(vars))
    }

    /// Every variable, in the order supplied.
    #[must_use]
    pub fn as_slice(&self) -> &[EnvVar] {
        &self.0
    }

    fn check(vars: &[EnvVar]) -> Result<(), TestEnvError> {
        if vars.len() > MAX_TEST_ENV {
            return Err(TestEnvError::TooMany);
        }
        let mut keys = BTreeSet::new();
        for var in vars {
            let repeated = !keys.insert(var.key());
            if repeated {
                return Err(TestEnvError::DuplicateKey);
            }
        }
        Ok(())
    }
}

/// What every proof binds besides the tree it runs over: the environment the
/// run happens in, the crate sources it builds against (ADR-0237
/// decisions 3 and 4), the cargo config that points cargo at them, and the
/// session-supplied test env, which reaches the test step only. The session
/// that offers a proof binds it, and the model never sees it.
///
/// The environment, vendor, and cargo config are typed citations, so the
/// driver's closure walk carries them into the invocation.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.bound")]
pub struct ProofBound {
    /// The run's whole root filesystem and its tool table, read from the head
    /// `(aether.workspace.environment, <platform>)`.
    environment: Ref<Environment>,
    /// The `cargo vendor` tree for the source's `Cargo.lock`, mounted
    /// read-only at `/vendor`: the `Vendored.tree` of a `vendor.cargo`
    /// transition over a source with the same `Cargo.lock`. An empty tree
    /// serves a workspace with no dependencies.
    vendor: Ref<Tree>,
    /// The tree holding `config.toml`, mounted read-only at `/.cargo`: every
    /// cargo in the run reads it as an ancestor of `/work`, and it replaces
    /// crates.io with `/vendor` offline. Staged beside the bound by
    /// [`cargo_config_artifacts`](super::cargo_config_artifacts).
    cargo_config: Ref<Tree>,
    /// The session-supplied variables the test step hands cargo. The fmt and
    /// clippy steps never see them.
    test_env: TestEnv,
}

impl ProofBound {
    /// Proofs that run in `environment`, build against the crate sources
    /// in `vendor` through the fixed cargo config, and hand `test_env` to the
    /// test step only.
    ///
    /// # Panics
    ///
    /// Never in practice: the fixed cargo config tree always encodes.
    #[must_use]
    pub fn new(environment: Ref<Environment>, vendor: Ref<Tree>, test_env: TestEnv) -> Self {
        let cargo_config = Ref::of_encoded(&config::tree()).expect("the fixed cargo config tree encodes");
        Self { environment, vendor, cargo_config, test_env }
    }

    /// The environment the run happens in.
    #[must_use]
    pub const fn environment(&self) -> Ref<Environment> {
        self.environment
    }

    /// The vendored crate sources, mounted at `/vendor`.
    #[must_use]
    pub const fn vendor(&self) -> Ref<Tree> {
        self.vendor
    }

    /// The cargo config tree, mounted at `/.cargo`.
    #[must_use]
    pub const fn cargo_config(&self) -> Ref<Tree> {
        self.cargo_config
    }

    /// The session-supplied variables, reaching the test step only.
    #[must_use]
    pub fn test_env(&self) -> &TestEnv {
        &self.test_env
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_workspace::EnvVar;
    use aether_data::wire::{decode_from_slice, encode_to_vec};

    use super::{
        MAX_SCOPE_ENTRIES, MAX_SCOPE_ENTRY_BYTES, MAX_TEST_ENV, ScopeEntries, ScopeEntriesError, ScopeEntry,
        ScopeEntryError, TestEnv, TestEnvError, TestScope,
    };

    fn distinct(count: usize) -> Vec<EnvVar> {
        (0..count).map(|index| EnvVar::new(format!("AETHER_TEST_{index}"), "1").expect("test variable")).collect()
    }

    /// `vars` refused by `new` with `error`, and by decode.
    fn refused(vars: Vec<EnvVar>, error: TestEnvError) {
        let bytes = encode_to_vec(&vars).expect("the inner list encodes");
        assert_eq!(TestEnv::new(vars), Err(error));
        assert!(decode_from_slice::<TestEnv>(&bytes).is_err(), "decode refuses what new refuses: {error}");
    }

    #[test]
    fn a_repeated_key_and_an_over_long_list_refuse_at_new_and_at_decode() {
        // Catches a bound that would hand cargo two values for one variable, or an unbounded list.
        let mut repeated = distinct(2);
        repeated.push(EnvVar::new("AETHER_TEST_0", "2").expect("test variable"));
        refused(repeated, TestEnvError::DuplicateKey);
        refused(distinct(MAX_TEST_ENV + 1), TestEnvError::TooMany);

        let most = TestEnv::new(distinct(MAX_TEST_ENV)).expect("the most entries are accepted");
        let bytes = encode_to_vec(&distinct(MAX_TEST_ENV)).expect("the inner list encodes");
        assert_eq!(decode_from_slice::<TestEnv>(&bytes).expect("decode accepts the most entries"), most);
    }

    /// `entry` refused by `new` with `error`, and by decode.
    fn refused_entry(entry: String, error: ScopeEntryError) {
        let bytes = encode_to_vec(&entry).expect("the inner string encodes");
        assert_eq!(ScopeEntry::new(entry), Err(error));
        assert!(decode_from_slice::<ScopeEntry>(&bytes).is_err(), "decode refuses what new refuses: {error}");
    }

    #[test]
    fn scope_entries_refuse_empty_long_bad_start_and_bad_char_at_new_and_at_decode() {
        // Catches unbounded or flag-injecting argv reaching cargo.
        refused_entry(String::new(), ScopeEntryError::Empty);
        refused_entry("a".repeat(MAX_SCOPE_ENTRY_BYTES + 1), ScopeEntryError::TooLong);
        for entry in ["-scoped", "-test", "--", "-"] {
            refused_entry(entry.to_owned(), ScopeEntryError::BadStart);
        }
        for entry in ["has space", "semi;colon", "back\\slash", "star*glob", "quote\"q"] {
            refused_entry(entry.to_owned(), ScopeEntryError::BadChar);
        }

        for entry in ["a", "_", "0", "session", "aether-bloomery-workspace", "gate", "a/b:c.d-e_f", "MuseSpark"] {
            let accepted = ScopeEntry::new(entry).expect("a neighbour accepts");
            assert_eq!(accepted.as_str(), entry);
            let bytes = encode_to_vec(&entry.to_owned()).expect("the inner string encodes");
            assert_eq!(decode_from_slice::<ScopeEntry>(&bytes).expect("decode accepts a neighbour"), accepted);
        }

        let longest = "a".repeat(MAX_SCOPE_ENTRY_BYTES);
        let accepted = ScopeEntry::new(longest.clone()).expect("the most bytes accept");
        let bytes = encode_to_vec(&longest).expect("the inner string encodes");
        assert_eq!(decode_from_slice::<ScopeEntry>(&bytes).expect("decode accepts the most bytes"), accepted);
    }

    #[test]
    fn scope_lists_refuse_past_the_max_and_accept_the_max() {
        // Catches an unbounded scope list reaching cargo's argv.
        let entries = |count: usize| {
            (0..count).map(|index| ScopeEntry::new(format!("target{index}")).expect("test entry")).collect::<Vec<_>>()
        };
        let bytes = encode_to_vec(&entries(MAX_SCOPE_ENTRIES + 1)).expect("the inner list encodes");
        assert_eq!(ScopeEntries::new(entries(MAX_SCOPE_ENTRIES + 1)), Err(ScopeEntriesError::TooMany));
        assert!(decode_from_slice::<ScopeEntries>(&bytes).is_err(), "decode refuses what new refuses");

        let most = ScopeEntries::new(entries(MAX_SCOPE_ENTRIES)).expect("the most entries accept");
        assert_eq!(most.as_slice().len(), MAX_SCOPE_ENTRIES);
        let bytes = encode_to_vec(&entries(MAX_SCOPE_ENTRIES)).expect("the inner list encodes");
        assert_eq!(decode_from_slice::<ScopeEntries>(&bytes).expect("decode accepts the most entries"), most);

        let empty = ScopeEntries::new(Vec::new()).expect("empty accepts");
        assert!(empty.as_slice().is_empty());
        assert!(TestScope::default().is_whole());
        let scoped = TestScope::new(
            ScopeEntries::new(vec![ScopeEntry::new("session").expect("entry")]).expect("targets"),
            ScopeEntries::default(),
        );
        assert!(!scoped.is_whole());
    }
}
