//! The compile-time mirror of a stored declaration.

use aether_bloomery_kinds::{Mode, ProgramName, Ref};
use aether_data::{Cites, Kind, Storage, StorageError};

use crate::kinds;

/// Typed mirror of a stored [`kinds::Program`].
///
/// [`declaration`] is the bridge: the stored form is the same bytes every
/// call, so the same digest.
pub trait Program {
    /// Must pass [`ProgramName`] rules; checked by [`declaration`] at first use.
    const NAME: &'static str;
    const MODE: Mode;
    const INTENT: &'static str;
    type Input: Storage + Clone + Cites;
    type Result: Storage + Clone + Cites;
}

/// The stored form. Same bytes every call, so the same digest.
///
/// # Panics
///
/// Panics if `P::NAME` is not a valid [`ProgramName`].
#[must_use]
pub fn declaration<P: Program>() -> kinds::Program {
    let name = ProgramName::new(P::NAME)
        .unwrap_or_else(|error| panic!("aether-bloomery-program: {:?} is not a ProgramName: {error}", P::NAME));
    kinds::Program { name, input: P::Input::ID, result: P::Result::ID, mode: P::MODE, intent: P::INTENT.to_owned() }
}

/// Digest of [`declaration`]. Same value every call.
///
/// # Panics
///
/// Panics if the declaration does not encode.
#[must_use]
pub fn digest<P: Program>() -> Ref<kinds::Program> {
    Ref::of_encoded(&declaration::<P>()).unwrap_or_else(|error: StorageError| {
        panic!("aether-bloomery-program: declaration of {:?} does not encode: {error}", P::NAME)
    })
}
