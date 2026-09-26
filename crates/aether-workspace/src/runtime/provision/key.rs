//! The run key an estimate is kept under (ADR-0237 decision 9, as amended by
//! #6777): what the run does, not who asked.

use std::fmt;

use aether_bloomery_kinds::{Digest, hash_bytes};

use crate::Run;

/// The domain tag every run key's hash input starts with.
const DOMAIN: &[u8] = b"aether.workspace.run-key.v1";

/// How many leading digest bytes [`RunKey`]'s `Display` shows.
const SHORT_BYTES: usize = 8;

/// The digest of a run's environment and its ordered steps, each step's
/// tool, args, and env. The tree, the mounts, the scratch paths, the network,
/// and every step's stdin are not in it: two runs that do the same thing
/// over different inputs share an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunKey(Digest);

impl RunKey {
    /// The key of `run`: sha256 over the domain tag, then the environment
    /// digest, the step count, and for each step in order its tool name, its
    /// args (count, then each), and its env (count, then each key and value,
    /// in the order given). Every count is a u64 LE and every byte string is
    /// prefixed by its length as one, so `["ab"]` and `["a", "b"]` differ.
    pub fn of(run: &Run) -> Self {
        let mut input = Vec::from(DOMAIN);
        field(&mut input, run.environment.digest().as_bytes());
        let steps = run.steps.as_slice();
        count(&mut input, steps.len());
        for step in steps {
            field(&mut input, step.tool.as_str().as_bytes());
            count(&mut input, step.args.len());
            for arg in &step.args {
                field(&mut input, arg.as_bytes());
            }
            count(&mut input, step.env.len());
            for var in &step.env {
                field(&mut input, var.key().as_bytes());
                field(&mut input, var.value().as_bytes());
            }
        }
        Self(hash_bytes(&input))
    }
}

impl fmt::Display for RunKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_bytes()[..SHORT_BYTES].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Append `len` as a u64 LE.
fn count(input: &mut Vec<u8>, len: usize) {
    input.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

/// Append `bytes` prefixed by its length.
fn field(input: &mut Vec<u8>, bytes: &[u8]) {
    count(input, bytes.len());
    input.extend_from_slice(bytes);
}
