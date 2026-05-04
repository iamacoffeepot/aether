//! Typed `MailboxId` constants for the well-known mailbox names in the
//! substrate vocabulary. The names themselves are scattered across the
//! codebase as `&'static str` constants (`AETHER_CONTROL`,
//! `HANDLE_MAILBOX_NAME`, `AETHER_DIAGNOSTICS`, etc.) — those keep their
//! string form for log-message interpolation. This module is the typed
//! companion: anywhere a `MailboxId` is being compared or constructed
//! against a well-known mailbox, prefer one of these constants over
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
