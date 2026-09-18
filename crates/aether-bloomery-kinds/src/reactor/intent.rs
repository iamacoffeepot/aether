//! One attributed reactor output carried in [`super::Evaluated::Completed`].

use alloc::vec::Vec;

use aether_data::KindId;

use crate::{ReactorName, RuleName};

/// One rule output: which reactor and rule produced it, and the mail encoding.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct ReactorIntent {
    reactor: ReactorName,
    rule: RuleName,
    kind: KindId,
    bytes: Vec<u8>,
}

impl ReactorIntent {
    /// Attribute `bytes` of mail kind `kind` to `reactor` / `rule`.
    #[must_use]
    pub fn new(reactor: ReactorName, rule: RuleName, kind: KindId, bytes: Vec<u8>) -> Self {
        Self { reactor, rule, kind, bytes }
    }

    /// Reactor that produced the output.
    #[must_use]
    pub const fn reactor(&self) -> &ReactorName {
        &self.reactor
    }

    /// Rule that produced the output.
    #[must_use]
    pub const fn rule(&self) -> &RuleName {
        &self.rule
    }

    /// Mail `Kind::ID` of the output.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// Mail-codec payload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Take the reactor, rule, kind, and payload.
    #[must_use]
    pub fn into_parts(self) -> (ReactorName, RuleName, KindId, Vec<u8>) {
        (self.reactor, self.rule, self.kind, self.bytes)
    }
}
