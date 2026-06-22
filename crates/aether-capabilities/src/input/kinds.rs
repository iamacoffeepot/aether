//! Mail kinds for the `aether.input` cap.
//!
//! Owns the six subscribe-control kinds the cap handles:
//! `SubscribeInput`, `SubscribeInputSelf`, `UnsubscribeInput`,
//! `UnsubscribeInputSelf`, `SubscribeInputResult`, `UnsubscribeAll`.
//!
//! The genuine input-stream event kinds (`Key`, `KeyRelease`,
//! `MouseMove`, `MouseButton`, `WindowSize`) stay in `aether-kinds`
//! because they are driver-produced core vocabulary consumed broadly
//! (and moving them would require upstream consumers to depend on
//! `aether-capabilities`).

use serde::{Deserialize, Serialize};

/// `aether.input.subscribe` — add `mailbox` to the subscriber set
/// for `kind`. Idempotent: subscribing a mailbox already in the
/// set is still `Ok` (subscriptions are a set, not a counter).
/// Reply: `SubscribeInputResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.input.subscribe")]
pub struct SubscribeInput {
    pub kind: aether_data::KindId,
    pub mailbox: aether_data::MailboxId,
}

/// `aether.input.subscribe_self` — reflexive counterpart of
/// [`SubscribeInput`]: subscribe the *sending* actor to the input
/// stream for `kind`, with no explicit `mailbox` field. The cap
/// resolves the subscriber from the inbound envelope's host-stamped
/// `Source` (ADR-0083) via `ctx.source_mailbox()`, so the
/// subscriber cannot be forged and the op is gated to in-process
/// actors by construction — an external session or another engine
/// has no local mailbox and gets an `Err` reply, pushing it onto
/// the named [`SubscribeInput`] form. This is the common
/// "subscribe me" case. Reply: `SubscribeInputResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.input.subscribe_self")]
pub struct SubscribeInputSelf {
    pub kind: aether_data::KindId,
}

/// `aether.input.unsubscribe` — remove `mailbox` from the
/// subscriber set for `kind`. Idempotent: unsubscribing a mailbox
/// that isn't subscribed is still `Ok`. Reply:
/// `SubscribeInputResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.input.unsubscribe")]
pub struct UnsubscribeInput {
    pub kind: aether_data::KindId,
    pub mailbox: aether_data::MailboxId,
}

/// `aether.input.unsubscribe_self` — reflexive counterpart of
/// [`UnsubscribeInput`]: unsubscribe the *sending* actor from the
/// input stream for `kind`, with no explicit `mailbox` field. The
/// cap resolves the subscriber from the inbound envelope's
/// host-stamped `Source` (ADR-0083), the same gating as
/// [`SubscribeInputSelf`]. Idempotent on "not currently
/// subscribed." Reply: `SubscribeInputResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.input.unsubscribe_self")]
pub struct UnsubscribeInputSelf {
    pub kind: aether_data::KindId,
}

/// Reply to subscribe / unsubscribe / `unsubscribe_all` (ADR-0021 §2).
/// Only failure mode: the target mailbox id doesn't name a live
/// component (unknown, a sink, or already dropped).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.input.subscribe_result")]
pub enum SubscribeInputResult {
    Ok,
    Err { error: String },
}

/// `aether.input.unsubscribe_all` — remove `mailbox` from every
/// input stream's subscriber set. Issued by
/// `ComponentHostCapability` on `DropComponent` so the cap's
/// fan-out tables don't keep firing at a dropped trampoline.
/// Idempotent: a mailbox with no subscriptions is still a no-op.
/// Fire-and-forget; no reply. Cast-shape (Pod) — one
/// `MailboxId`, fixed size.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.input.unsubscribe_all")]
pub struct UnsubscribeAll {
    pub mailbox: aether_data::MailboxId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;
    use aether_kinds::descriptors;

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(SubscribeInput::NAME, "aether.input.subscribe");
        assert_eq!(SubscribeInputSelf::NAME, "aether.input.subscribe_self");
        assert_eq!(UnsubscribeInput::NAME, "aether.input.unsubscribe");
        assert_eq!(UnsubscribeInputSelf::NAME, "aether.input.unsubscribe_self");
        assert_eq!(SubscribeInputResult::NAME, "aether.input.subscribe_result");
        assert_eq!(UnsubscribeAll::NAME, "aether.input.unsubscribe_all");
    }

    /// Confirms the `inventory::submit!` in each kind's `Kind` derive
    /// rides the move into `aether-capabilities` — `descriptors::all()`
    /// returns the six `aether.input.*` names when the cap crate is
    /// linked into the test binary (ADR-0121).
    #[test]
    fn moved_kinds_register_in_descriptor_inventory() {
        let all = descriptors::all();
        let names: Vec<&str> = all.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&SubscribeInput::NAME),
            "aether.input.subscribe missing from descriptor inventory",
        );
        assert!(
            names.contains(&SubscribeInputSelf::NAME),
            "aether.input.subscribe_self missing from descriptor inventory",
        );
        assert!(
            names.contains(&UnsubscribeInput::NAME),
            "aether.input.unsubscribe missing from descriptor inventory",
        );
        assert!(
            names.contains(&UnsubscribeInputSelf::NAME),
            "aether.input.unsubscribe_self missing from descriptor inventory",
        );
        assert!(
            names.contains(&SubscribeInputResult::NAME),
            "aether.input.subscribe_result missing from descriptor inventory",
        );
        assert!(
            names.contains(&UnsubscribeAll::NAME),
            "aether.input.unsubscribe_all missing from descriptor inventory",
        );
    }
}
