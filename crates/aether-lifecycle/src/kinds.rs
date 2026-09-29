//! The `aether.lifecycle` subscription request vocabulary (ADR-0082 §7).
//!
//! An explicit request names its subscriber by a
//! [`ProtocolPath<Subscriber<K>>`](aether_actor::ProtocolPath), where `K` is
//! the stage it subscribes to, so the path decodes only against a route that
//! handles that stage silently (ADR-0231 §3, §8). A reflexive request names
//! no subscriber: the cap types its sender with the guard cast.

use aether_actor::{ProtocolPath, Subscriber};
use aether_kinds::{InitCaps, InitComponents, Present, Render, Shutdown, Tick};

/// Writes [`LifecycleSubscription`] from the stage list.
macro_rules! subscription {
    ($($stage:ident $field:ident),+ $(,)?) => {
        /// One published stage and the subscriber to hold for it: the path of
        /// an actor that handles the stage silently (ADR-0231 §8).
        ///
        /// Over MCP each variant takes the subscriber's canonical path, as in
        /// `{"Tick": "aether.kit.camera"}`.
        #[derive(aether_data::Schema, Debug, Clone, PartialEq, Eq)]
        pub enum LifecycleSubscription {
            $(
                #[doc = concat!("A subscriber to [`", stringify!($stage), "`].")]
                $stage(ProtocolPath<Subscriber<$stage>>),
            )+
        }
    };
}

published_stages!(subscription);

/// Subscribe an explicitly named actor to a lifecycle stage broadcast
/// (ADR-0082 §7). `subscription` names the stage and the subscriber's
/// canonical path, which decodes only when the actor there handles the stage
/// silently (ADR-0231 §3); the cap proves it live at receipt. Substrate
/// replies with [`LifecycleSubscribeResult`] — `Err` when the chassis's
/// lifecycle graph doesn't declare the stage (fail-fast at wire time per
/// ADR-0082 §7) or when the subscriber is no longer live.
#[aether_data::kind(name = "aether.lifecycle.subscribe", no_serde, eq)]
pub struct LifecycleSubscribe {
    pub subscription: LifecycleSubscription,
}

/// Reflexive counterpart of [`LifecycleSubscribe`]: subscribe the
/// *sending* actor to a lifecycle stage broadcast, with no explicit
/// subscriber. The cap resolves the subscriber from the inbound
/// envelope's host-stamped `Source` (ADR-0083) via
/// `ctx.sender()`, so the subscriber cannot be forged and the
/// op is gated to in-process actors by construction — an external
/// session or another engine has no local mailbox and gets an `Err`
/// reply, pushing it onto the named [`LifecycleSubscribe`] form. This
/// is the common "subscribe me" case; `stage` carries the
/// [`KindId`](aether_data::KindId) of the stage kind (e.g.
/// `<Tick as Kind>::ID.0`), and the sender must handle that stage silently
/// or manually. Substrate replies with [`LifecycleSubscribeResult`].
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.subscribe_self", pod, default, eq)]
pub struct LifecycleSubscribeSelf {
    pub stage: u64,
}

/// Unsubscribe counterpart of [`LifecycleSubscribe`]: `subscription` names
/// the stage and the same subscriber path. Idempotent on "not currently
/// subscribed", which includes a subscriber that is no longer live — a
/// closed subscriber already left every stage through its `MonitorNotice`.
#[aether_data::kind(name = "aether.lifecycle.unsubscribe", no_serde, eq)]
pub struct LifecycleUnsubscribe {
    pub subscription: LifecycleSubscription,
}

/// Reflexive counterpart of [`LifecycleUnsubscribe`]: unsubscribe the
/// *sending* actor from a lifecycle stage, with no explicit subscriber. The
/// cap resolves the subscriber from the inbound envelope's host-stamped
/// `Source` (ADR-0083), the same gating as [`LifecycleSubscribeSelf`].
/// Idempotent on "not currently subscribed." Substrate replies with
/// [`LifecycleSubscribeResult`].
#[repr(C)]
#[aether_data::kind(name = "aether.lifecycle.unsubscribe_self", pod, default, eq)]
pub struct LifecycleUnsubscribeSelf {
    pub stage: u64,
}

/// Reply to the four subscription requests. `Err` carries the stage kind id
/// and a human-readable reason — fail-fast subscribe per ADR-0082 §7. Same
/// shape and rationale as `SubscribeWindowResult` for window-event
/// subscriptions.
#[aether_data::kind(name = "aether.lifecycle.subscribe_result")]
pub enum LifecycleSubscribeResult {
    Ok,
    Err { stage: u64, error: String },
}
