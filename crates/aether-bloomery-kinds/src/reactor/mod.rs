//! Reactor-set membership and the mail a driver exchanges with a reactor bundle.

mod declaration;
mod intent;
mod mail;
mod set;

pub use declaration::{
    REACTORS_SECTION, ReactorDeclaration, ReactorDeclarationError, ReactorDeclarationsError, RuleDeclaration,
    RuleRecord, reactor_declarations, reactor_record_len, write_reactor_record,
};
pub use intent::ReactorIntent;
pub use mail::{Evaluated, Event, Status, StatusQuery, Warm, WarmEntries, WarmEntriesError, Warmed};
pub use set::{ReactorSet, ReactorSetError};
