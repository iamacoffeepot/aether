//! Reactor-set membership and the mail a driver exchanges with a reactor bundle.

mod intent;
mod mail;
mod set;

pub use intent::ReactorIntent;
pub use mail::{Evaluated, Event, Status, StatusQuery, Warm, WarmEntries, WarmEntriesError, Warmed};
pub use set::{ReactorSet, ReactorSetError};
