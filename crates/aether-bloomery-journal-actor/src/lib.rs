//! A named native actor owns each Bloomery journal and answers read/head mail.
//!
//! Spawn one [`JournalActor`] per journal path with a distinct named subname.
//! The actor holds the only mutable journal handle for its file; future writes
//! must enter through this owner's serialized mailbox.

mod runtime;

pub use runtime::JournalActor;
