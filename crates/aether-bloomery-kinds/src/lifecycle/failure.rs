//! A reactor reaction that produced no records for its trigger.

use crate::{Detail, Digest, ReactorName};

/// A reactor reaction that failed. Written only by the driver, caused by
/// the trigger seq. `reactor: None` is a poisoned or protocol-level
/// failure of the whole bundle; `Some` names one failed reactor and leaves
/// the bundle's other reactors unaffected.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.reaction_failed")]
pub struct ReactionFailed {
    pub bundle: Digest,
    pub reactor: Option<ReactorName>,
    pub reason: Detail,
}
