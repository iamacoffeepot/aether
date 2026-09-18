//! The compile-time mirror of a stored declaration.

use alloc::string::String;

use aether_bloomery_kinds::{Mode, ProgramName, Refusal};
use aether_data::{Cites, Kind, Storage};

use crate::env::{Env, Pure};
use crate::kinds;

/// Typed mirror of a stored [`kinds::Program`].
///
/// [`declaration`] is the bridge: the stored form is the same bytes every
/// call. Programs are stateless by signature.
pub trait Program {
    /// Must pass [`ProgramName`] rules; checked by [`declaration`] at first use.
    const NAME: &'static str;
    const MODE: Mode;
    const INTENT: &'static str;
    type Input: Storage + Clone + Cites;
    type Result: Storage + Clone + Cites;

    /// Run once over `input` and the injected environment.
    fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal>;
}

/// The stored form. Same bytes every call.
///
/// # Panics
///
/// Panics if `P::NAME` is not a valid [`ProgramName`].
#[must_use]
pub fn declaration<P: Program>() -> kinds::Program {
    let name = ProgramName::new(P::NAME)
        .unwrap_or_else(|error| panic!("aether-bloomery-program: {:?} is not a ProgramName: {error}", P::NAME));
    kinds::Program { name, input: P::Input::ID, result: P::Result::ID, mode: P::MODE, intent: String::from(P::INTENT) }
}
