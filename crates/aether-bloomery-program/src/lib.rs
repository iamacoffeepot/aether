//! Portable guest SDK for bloomery WASM programs.
//!
//! A program is a stateless function over an injected closure, the same
//! injected-data sandbox reactors use. The native driver sends
//! [`Invoke`]; [`invoke()`] runs one [`Program`] and replies [`Invoked`].
//! Only native code writes journal records. A program's identity is its
//! bundle digest plus name, not a stored declaration digest. [`Root`] is the
//! bundle root's state: the program table plus the live-seq table.
//!
//! `#![no_std]` + `alloc`. Guests cannot link the journal.

#![no_std]

extern crate alloc;
extern crate self as aether_bloomery_program;

mod declare;
mod env;
mod invoke;
mod root;
mod section;

pub use aether_bloomery_kinds as kinds;
pub use aether_bloomery_kinds::{Invoke, Invoked, Refusal};
pub use aether_bloomery_program_derive::program;
pub use declare::Program;
pub use env::{Env, Pure};
pub use invoke::{invoke, unreachable_staged};
pub use root::{Admission, ProgramEntry, ProgramTable, Root, dispatch};
pub use section::{DeclarationsError, declarations};

#[doc(hidden)]
pub mod __macro_internals {
    pub use aether_data::Kind;
    pub use alloc::string::ToString;
    pub use alloc::vec::Vec;

    pub use crate::root::program_table;
    pub use crate::section::{MODE_PURE, program_record_len, write_program_record};
}
