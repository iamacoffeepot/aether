//! Views over a contiguous journal prefix.
//!
//! [`Heads`] is the typed last-move fold. A [`View`] consumes every entry in a
//! batch, including kinds it ignores. There is no `Clone` / `Send` / `Sync`
//! bound. This crate is `no_std` + `alloc` and does not own a journal: callers
//! supply entries, and the owner of the fold enforces cursor and failure
//! handling.
//!
//! ```
//! use aether_bloomery_kinds::Seq;
//! use aether_bloomery_view::Heads;
//!
//! let heads = Heads::new();
//! assert_eq!(heads.cursor(), Seq(0));
//! ```

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod heads;
mod view;

pub use heads::{HeadFoldError, Heads};
pub use view::View;
