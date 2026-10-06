//! The run key an estimate is kept under (ADR-0237 decision 9, as amended by
//! #6777): what the run does, not who asked.

use std::fmt;

use aether_data::Digest;

use crate::{RunRequest, run_key};

/// How many leading digest bytes [`RunKey`]'s `Display` shows.
const SHORT_BYTES: usize = 8;

/// The digest of a run's environment and its ordered steps, each step's
/// tool, args, and env. The tree, the mounts, the scratch paths, the network,
/// every step's stdin, and the guest layer are not in it: two runs that do
/// the same thing over different inputs share an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunKey(Digest);

impl RunKey {
    /// The key of `run`: sha256 over the domain tag, then the environment
    /// digest, the step count, and for each step in order its tool name, its
    /// args (count, then each), and its env (count, then each key and value,
    /// in the order given). Every count is a u64 LE and every byte string is
    /// prefixed by its length as one, so `["ab"]` and `["a", "b"]` differ.
    pub fn of(run: &RunRequest) -> Self {
        Self(run_key(run.environment.digest(), &run.steps))
    }

    /// The whole digest, where [`fmt::Display`] shows only its start.
    pub fn digest(&self) -> Digest {
        self.0
    }
}

impl fmt::Display for RunKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_bytes()[..SHORT_BYTES].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}
