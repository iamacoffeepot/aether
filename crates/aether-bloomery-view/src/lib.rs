//! Lazy native views over a contiguous journal prefix.
//!
//! [`Heads`] is the typed last-move fold. [`ViewRegistry`] caches one instance
//! per native [`std::any::TypeId`], catches each selected view up to an exact
//! sequence, and injects immutable references into a synchronous callback:
//!
//! ```
//! use aether_bloomery_journal::{Journal, Seq, SystemClock};
//! use aether_bloomery_view::{Heads, ViewRegistry};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let journal = Journal::open_in_memory_with_clock(Box::new(SystemClock))?;
//! let mut registry = ViewRegistry::new(journal);
//! let cursor = registry.views::<Heads>().at(Seq(0)).with(|heads| heads.cursor())?;
//! assert_eq!(cursor, Seq(0));
//! # Ok(())
//! # }
//! ```
//!
//! `views` and `at` do no replay. Existing program execution appends through
//! [`ViewRegistry::journal_mut`]; successful cached state is reused on the
//! next request.

#![forbid(unsafe_code)]

mod error;
mod heads;
mod registry;
mod selection;
mod view;

pub use error::ViewError;
pub use heads::{HeadFoldError, Heads};
pub use registry::{Positioned, Unpositioned, ViewRegistry, Views};
pub use selection::ViewSelection;
pub use view::View;
