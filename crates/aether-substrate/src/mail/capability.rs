// Queryable capability registry (iamacoffeepot/aether#1037).
//
// A sibling of the routing [`Registry`](crate::mail::registry::Registry)
// — NOT folded into it. The routing registry is on the per-mail hot
// path (recipient + kind resolution on every `Mailer::push`); the
// capability metadata here is read only on the DAG validator's
// submit/validate path (iamacoffeepot/aether#975 Phase 2), so it lives
// off the dispatch struct.
//
// The surface is **input-side only**: which kinds a mailbox accepts
// (`accepts`) and whether it carries a `#[fallback]` catch-all
// (`has_fallback`). Handlers promise nothing about what they reply, so
// there is deliberately no reply-kind / output-kind resolution here —
// a request kind's reply kind is not a property of the kind and is not
// declarable or queryable (see ADR-0047 §3, amended).
//
// Population is unified across native caps and wasm components: both
// surface a `ComponentCapabilities` (ADR-0033) — wasm from the parsed
// `aether.kinds.inputs` custom section, native from the always-on
// `__AETHER_INPUTS_MANIFEST` the `#[actor]` macro emits — so a single
// `register` call covers both. Register on add (component load /
// native-cap boot), replace on `aether.component.replace` (the mailbox
// id is stable per ADR-0022), remove on drop.

// The registry's `RwLock` guard is intentionally held across the
// read-then-membership-check pair in `accepts` — the same low-contention
// rationale as the routing registry's guard policy.
#![allow(clippy::significant_drop_tightening)]

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use aether_kinds::ComponentCapabilities;

use crate::mail::{KindId, MailboxId};

/// The accepted-kinds set + fallback flag for one mailbox. Built from
/// the mailbox's [`ComponentCapabilities`] (ADR-0033) at registration.
#[derive(Debug, Default, Clone)]
pub struct MailboxCaps {
    /// Every kind id the mailbox has a `#[handler]` for.
    pub handlers: HashSet<KindId>,
    /// Whether the mailbox carries a `#[fallback]` catch-all. A
    /// fallback mailbox accepts any kind at dispatch time even though
    /// the kind isn't in `handlers`.
    pub has_fallback: bool,
}

impl MailboxCaps {
    /// Project a [`ComponentCapabilities`] (the ADR-0033 surface shared
    /// by native caps and wasm components) into the dispatchability
    /// shape this registry stores: the handler kind-id set + fallback
    /// presence. Handler names and docs are dropped — they're
    /// `describe_component`'s concern, not the validator's.
    #[must_use]
    pub fn from_component_capabilities(caps: &ComponentCapabilities) -> Self {
        Self {
            handlers: caps.handlers.iter().map(|h| h.id).collect(),
            has_fallback: caps.fallback.is_some(),
        }
    }
}

/// Substrate-owned, queryable capability registry. Shared as
/// `Arc<CapabilityRegistry>` off the boot path (mirroring how the
/// routing [`Registry`](crate::mail::registry::Registry) is shared);
/// the DAG validator reads `accepts` / `has_fallback` on the submit
/// path, the load / replace / drop hooks mutate it.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    caps: RwLock<HashMap<MailboxId, MailboxCaps>>,
}

impl CapabilityRegistry {
    /// A fresh, empty registry. The boot path builds one and shares it
    /// via the [`Mailer`](crate::mail::mailer::Mailer).
    #[must_use]
    pub fn new() -> Self {
        Self {
            caps: RwLock::new(HashMap::new()),
        }
    }

    /// Does `mailbox` accept `kind`? True when `kind` is in the
    /// mailbox's handler set OR the mailbox carries a `#[fallback]`
    /// catch-all. Unknown mailboxes (never registered, or dropped)
    /// accept nothing.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned — a poisoned lock means
    /// a prior writer panicked mid-update, which is a substrate-level
    /// invariant violation (fail-fast per ADR-0063).
    #[must_use]
    pub fn accepts(&self, mailbox: MailboxId, kind: KindId) -> bool {
        let guard = self.caps.read().expect("capability registry lock poisoned");
        guard
            .get(&mailbox)
            .is_some_and(|c| c.has_fallback || c.handlers.contains(&kind))
    }

    /// Does `mailbox` carry a `#[fallback]` catch-all? Unknown
    /// mailboxes return `false`.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::accepts`]).
    #[must_use]
    pub fn has_fallback(&self, mailbox: MailboxId) -> bool {
        let guard = self.caps.read().expect("capability registry lock poisoned");
        guard.get(&mailbox).is_some_and(|c| c.has_fallback)
    }

    /// Register (or replace) the caps for `mailbox`. Called at
    /// component load / native-cap boot, and again on
    /// `aether.component.replace` (same mailbox id, fresh handler set).
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::accepts`]).
    pub fn register(&self, mailbox: MailboxId, caps: MailboxCaps) {
        let mut guard = self
            .caps
            .write()
            .expect("capability registry lock poisoned");
        guard.insert(mailbox, caps);
    }

    /// Remove `mailbox`'s caps. Called on `aether.component.drop`. A
    /// no-op for an unknown mailbox.
    ///
    /// # Panics
    /// Panics if the internal lock is poisoned (see [`Self::accepts`]).
    pub fn remove(&self, mailbox: MailboxId) {
        let mut guard = self
            .caps
            .write()
            .expect("capability registry lock poisoned");
        guard.remove(&mailbox);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::{FallbackCapability, HandlerCapability};

    fn caps_with(handler_ids: &[u64], fallback: bool) -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: handler_ids
                .iter()
                .map(|&id| HandlerCapability {
                    id: KindId(id),
                    name: format!("test.kind.{id}"),
                    doc: None,
                })
                .collect(),
            fallback: fallback.then_some(FallbackCapability { doc: None }),
            doc: None,
            config: None,
        }
    }

    #[test]
    fn accepts_handled_kind_rejects_others() {
        let reg = CapabilityRegistry::new();
        let mbx = MailboxId(7);
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[10, 20], false)),
        );
        assert!(reg.accepts(mbx, KindId(10)));
        assert!(reg.accepts(mbx, KindId(20)));
        assert!(!reg.accepts(mbx, KindId(30)));
    }

    #[test]
    fn fallback_accepts_anything() {
        let reg = CapabilityRegistry::new();
        let mbx = MailboxId(7);
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[10], true)),
        );
        assert!(reg.has_fallback(mbx));
        assert!(reg.accepts(mbx, KindId(10)));
        // Not in `handlers`, but the fallback catches it.
        assert!(reg.accepts(mbx, KindId(999)));
    }

    #[test]
    fn strict_receiver_has_no_fallback() {
        let reg = CapabilityRegistry::new();
        let mbx = MailboxId(7);
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[10], false)),
        );
        assert!(!reg.has_fallback(mbx));
        assert!(!reg.accepts(mbx, KindId(999)));
    }

    #[test]
    fn register_replaces_prior_caps() {
        let reg = CapabilityRegistry::new();
        let mbx = MailboxId(7);
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[10], false)),
        );
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[20], false)),
        );
        assert!(!reg.accepts(mbx, KindId(10)));
        assert!(reg.accepts(mbx, KindId(20)));
    }

    #[test]
    fn remove_clears_caps() {
        let reg = CapabilityRegistry::new();
        let mbx = MailboxId(7);
        reg.register(
            mbx,
            MailboxCaps::from_component_capabilities(&caps_with(&[10], true)),
        );
        reg.remove(mbx);
        assert!(!reg.accepts(mbx, KindId(10)));
        assert!(!reg.has_fallback(mbx));
    }

    #[test]
    fn unknown_mailbox_accepts_nothing() {
        let reg = CapabilityRegistry::new();
        assert!(!reg.accepts(MailboxId(123), KindId(10)));
        assert!(!reg.has_fallback(MailboxId(123)));
    }
}
