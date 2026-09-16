//! Views over a contiguous journal prefix.
//!
//! [`Heads`] is the typed last-move fold. With the default `native` feature,
//! `ViewRegistry` caches one instance per native `std::any::TypeId`,
//! catches each selected view up to an exact sequence, and injects immutable
//! references into a synchronous callback:
//!
//! ```
//! # #[cfg(not(feature = "native"))]
//! # fn main() {}
//! # #[cfg(feature = "native")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use aether_bloomery_journal::{Journal, Seq, SystemClock};
//! use aether_bloomery_view::{Heads, ViewRegistry};
//!
//! let journal = Journal::open_in_memory_with_clock(Box::new(SystemClock))?;
//! let mut registry = ViewRegistry::new(journal);
//! let cursor = registry.views::<Heads>().at(Seq(0)).with(|heads| heads.cursor())?;
//! assert_eq!(cursor, Seq(0));
//! # Ok(())
//! # }
//! ```
//!
//! `views` and `at` do no replay. Existing program execution appends through
//! `ViewRegistry::journal_mut`; successful cached state is reused on the
//! next request.
//!
//! Without default features this crate is `no_std` + `alloc` and exposes
//! [`View`] and [`Heads`] so a caller can fold supplied journal entries.

#![cfg_attr(not(feature = "native"), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "native")]
mod error;
mod heads;
#[cfg(feature = "native")]
mod registry;
#[cfg(feature = "native")]
mod selection;
mod view;

#[cfg(feature = "native")]
pub use error::ViewError;
pub use heads::{HeadFoldError, Heads};
#[cfg(feature = "native")]
pub use registry::{Positioned, Unpositioned, ViewRegistry, Views};
#[cfg(feature = "native")]
pub use selection::ViewSelection;
pub use view::View;
