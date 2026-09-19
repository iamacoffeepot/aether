//! Typed programs; `#[program]` refuses an invalid `NAME` at compile time.

use aether_bloomery_kinds::{Mode, Refusal};
use aether_data::{Cites, Storage};

use crate::env::{Env, Pure};

/// Typed mirror of a stored [`crate::kinds::Program`].
///
/// `#[program]` proves the stored form at compile time: the section record
/// carries the declared `NAME`, `INTENT`, input and result. Programs are
/// stateless by signature.
pub trait Program {
    /// Must pass [`crate::kinds::ProgramName`] rules; `#[program]` refuses an invalid `NAME` at compile time.
    const NAME: &'static str;
    const MODE: Mode;
    const INTENT: &'static str;
    type Input: Storage + Clone + Cites;
    type Result: Storage + Clone + Cites;

    /// Run once over `input` and the injected environment.
    fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal>;
}
