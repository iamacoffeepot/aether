//! What a proof tool takes: the arguments the model writes for each proof,
//! and the session values every proof binds, the environment, the vendor
//! tree, the cargo config, and the test env.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{Ref, Tree};
use aether_bloomery_workspace::{EnvVar, Environment};
use aether_data::Invariant;

use super::config;

/// The arguments of `proof.clippy`: none. The proof always runs over the
/// whole workspace in the session's tree, so the model writes `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.clippy.args")]
pub struct ClippyArgs;

/// The arguments of `proof.test`: none. The proof always runs over the
/// whole workspace in the session's tree, so the model writes `{}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.test.args")]
pub struct TestArgs;

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

    use super::{MAX_TEST_ENV, TestEnv, TestEnvError};

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
}
