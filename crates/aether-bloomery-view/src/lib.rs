//! Views over a contiguous journal prefix.
//!
//! [`Heads`] is the typed last-move fold. [`HeadHistory`] keeps every move,
//! so it answers the [`Heads`] of any earlier seq it has folded. [`Requests`]
//! and [`Activations`] fold the ADR-0226 driver records: outstanding program
//! requests and their dedup key, and per-head activation state. A [`View`]
//! consumes every entry in a batch, including kinds it ignores. There is no
//! `Clone` / `Send` / `Sync` bound. [`Publish`] is the opt-in owned snapshot
//! contract; it is not required of every view. This crate is `no_std` +
//! `alloc` and does not own a journal: callers supply entries, and the owner
//! of the fold enforces cursor and failure handling.
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

mod activations;
mod heads;
mod history;
mod publish;
mod requests;
mod selection;
mod sequence;
mod view;

pub use activations::{ActivationFoldError, Activations, HeadActivation};
pub use heads::{HeadFoldError, Heads};
pub use history::HeadHistory;
pub use publish::{Publish, PublishError};
pub use requests::{Outcome, Request, RequestFoldError, Requests};
pub use selection::{SelectedReactor, SelectionError, select_reactors};
pub use sequence::SequenceError;
pub use view::View;
