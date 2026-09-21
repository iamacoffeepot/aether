//! Typed programs; `#[program]` refuses an invalid `NAME` at compile time.

use core::future::Future;

use aether_bloomery_kinds::{Mode, Refusal};
use aether_data::{Cites, Storage};

use crate::env::{Async, Env, Sync};

/// Typed mirror of a stored [`crate::kinds::Program`].
///
/// `#[program]` proves the stored form at compile time: the section record
/// carries the declared `NAME`, `INTENT`, input and result. Programs are
/// stateless by signature. `run` lives on a private sync or async supertrait
/// because Rust cannot overload the two forms.
pub trait Program {
    /// Must pass [`crate::kinds::ProgramName`] rules; `#[program]` refuses an invalid `NAME` at compile time.
    const NAME: &'static str;
    const MODE: Mode;
    const INTENT: &'static str;
    type Input: Storage + Clone + Cites + Send + 'static;
    type Result: Storage + Clone + Cites + Send + 'static;
}

/// Sync `fn run` over [`Env<Sync>`]. Authors do not implement this; `#[program]` does.
#[doc(hidden)]
pub trait SyncProgram: Program {
    /// Run once over `input` and the injected environment.
    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal>;
}

/// Async `async fn run` over [`Env<Async>`]. Authors do not implement this; `#[program]` does.
#[doc(hidden)]
pub trait AsyncProgram: Program {
    /// Run once over `input` and an environment that may fetch missing artifacts.
    fn run(input: Self::Input, env: Env<Async>)
    -> impl Future<Output = Result<Self::Result, Refusal>> + Send + 'static;
}
