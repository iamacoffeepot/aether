//! Portable guest SDK for bloomery WASM programs.
//!
//! A program is a stateless function over an injected closure, the same
//! injected-data sandbox reactors use. The native driver sends
//! [`Invoke`]; [`invoke()`] runs one [`Program`] and replies [`Invoked`].
//! Only native code writes journal records. A program's identity is its
//! bundle digest plus name, not a stored declaration digest.
//!
//! `#![no_std]` + `alloc`. Guests cannot link the journal.

#![no_std]

extern crate alloc;

mod declare;
mod env;
mod invoke;

pub use aether_bloomery_kinds as kinds;
pub use aether_bloomery_kinds::{Invoke, Invoked, Refusal};
pub use declare::{Program, declaration};
pub use env::{Env, Pure};
pub use invoke::invoke;
