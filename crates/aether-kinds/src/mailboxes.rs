//! Typed `MailboxId` constants for the well-known mailbox names in the
//! substrate vocabulary. Substrate-side capabilities each declare
//! their `&'static str` recipient name as `<X>Capability::NAMESPACE`
//! (issue 525 Phase 1); the SDK side mirrors them as free
//! `pub const X_MAILBOX_NAME` strings (`aether-component::io`,
//! `aether-component::net`, `aether-actor::handle`) for use in
//! `Mailbox::resolve(...)` calls. This module is the typed companion:
//! anywhere a `MailboxId` is being compared or constructed against a
//! well-known mailbox, prefer one of these constants over
//! `mailbox_id_from_name("...")` so the resolution happens at compile
//! time and the call site is one symbol instead of a name + hash
//! invocation.
//!
//! Adding a new chassis-owned mailbox: pick the `aether.<name>` form,
//! mirror it as a `pub const` here, and the substrate side stays in
//! lockstep. (ADR-0074 Phase 5 retired the `aether.sink.*` namespace —
//! chassis-owned mailboxes now address as `aether.<name>` directly.)

use aether_data::{MailboxId, mailbox_id_from_name};
/// ADR-0010 control plane mailbox. Every load / drop / replace /
/// subscribe-input / unsubscribe-input lands here.
pub const CONTROL: MailboxId = mailbox_id_from_name("aether.control");

/// ADR-0023 diagnostics mailbox. Hub-side observation traffic
/// (engine_logs, frame_stats, etc.) is addressed here.
pub const DIAGNOSTICS: MailboxId = mailbox_id_from_name("aether.diagnostics");

/// Render mailbox (chassis-owned). Both `aether.draw_triangle` and
/// `aether.camera` (view_proj) flow here per ADR-0074 §Decision 7 —
/// the pre-Phase-3 `aether.sink.camera` mailbox folded into render as
/// part of the unified-actor refactor; Phase 5 then retired the
/// `aether.sink.*` namespace, so the mailbox is addressed as
/// `aether.render` directly.
pub const RENDER: MailboxId = mailbox_id_from_name("aether.render");

/// Audio mailbox (desktop chassis). NoteOn / NoteOff / SetMasterGain
/// (ADR-0039) route here.
pub const AUDIO: MailboxId = mailbox_id_from_name("aether.audio");

/// File I/O mailbox (ADR-0041). Read / Write / Delete / List addressed
/// against logical namespaces flow through this mailbox.
pub const IO: MailboxId = mailbox_id_from_name("aether.io");

/// Network mailbox (ADR-0043). Fetch / Cancel route here.
pub const NET: MailboxId = mailbox_id_from_name("aether.net");

/// Log mailbox — components emit `LogEvent` here for structured logging
/// that surfaces through `engine_logs`.
pub const LOG: MailboxId = mailbox_id_from_name("aether.log");

/// Handle store mailbox (ADR-0045). Publish / Drop / Resolve route
/// against the substrate-owned refcounted byte cache.
pub const HANDLE: MailboxId = mailbox_id_from_name("aether.handle");

/// Hub broadcast mailbox name (ADR-0008 observation path). Issue 576
/// promoted broadcast into a real catch-all chassis cap living in
/// `aether-capabilities`; substrate-internal code (scheduler death
/// paths, frame loop frame_stats push) keeps a typed handle on the
/// id without depending on the capabilities crate, so the constant
/// lives here in `aether-kinds` — the layer both `aether-substrate`
/// and `aether-capabilities` already pull. The cap reuses
/// `HUB_BROADCAST_MAILBOX_NAME` as its `Actor::NAMESPACE` so name and
/// id stay in lockstep without a second source of truth.
pub const HUB_BROADCAST_MAILBOX_NAME: &str = "hub.claude.broadcast";

/// Const-evaluated mailbox id matching [`HUB_BROADCAST_MAILBOX_NAME`].
/// Same value any `mailbox_id_from_name` lookup at this name lands at,
/// folded into a single symbol so chassis code reaches one place
/// instead of recomputing the hash.
pub const HUB_BROADCAST: MailboxId = mailbox_id_from_name(HUB_BROADCAST_MAILBOX_NAME);
