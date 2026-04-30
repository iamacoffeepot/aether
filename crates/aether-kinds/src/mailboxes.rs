//! Typed `MailboxId` constants for the well-known mailbox names in the
//! substrate vocabulary. The names themselves are scattered across the
//! codebase as `&'static str` constants (`AETHER_CONTROL`,
//! `HANDLE_SINK_NAME`, `AETHER_DIAGNOSTICS`, etc.) — those keep their
//! string form for log-message interpolation. This module is the typed
//! companion: anywhere a `MailboxId` is being compared or constructed
//! against a well-known mailbox, prefer one of these constants over
//! `mailbox_id_from_name("...")` so the resolution happens at compile
//! time and the call site is one symbol instead of a name + hash
//! invocation.
//!
//! Adding a new chassis-owned sink: pick the `aether.sink.*` name,
//! mirror it as a `pub const` here, and the substrate side stays in
//! lockstep.

use aether_data::{MailboxId, mailbox_id_from_name};
/// ADR-0010 control plane mailbox. Every load / drop / replace /
/// subscribe-input / unsubscribe-input lands here.
pub const CONTROL: MailboxId = mailbox_id_from_name("aether.control");

/// ADR-0023 diagnostics mailbox. Hub-side observation traffic
/// (engine_logs, frame_stats, etc.) is addressed here.
pub const DIAGNOSTICS: MailboxId = mailbox_id_from_name("aether.diagnostics");

/// Render sink (chassis-owned). DrawTriangle, etc. flow here per
/// ADR-0058 namespace conventions.
pub const SINK_RENDER: MailboxId = mailbox_id_from_name("aether.sink.render");

/// Camera sink (chassis-owned). `aether.camera { view_proj }` lands
/// here; latest value wins per tick.
pub const SINK_CAMERA: MailboxId = mailbox_id_from_name("aether.sink.camera");

/// Audio sink (desktop chassis). NoteOn / NoteOff / SetMasterGain
/// (ADR-0039) route here.
pub const SINK_AUDIO: MailboxId = mailbox_id_from_name("aether.sink.audio");

/// File I/O sink (ADR-0041). Read / Write / Delete / List addressed
/// against logical namespaces flow through this mailbox.
pub const SINK_IO: MailboxId = mailbox_id_from_name("aether.sink.io");

/// Network sink (ADR-0043). Fetch / Cancel route here.
pub const SINK_NET: MailboxId = mailbox_id_from_name("aether.sink.net");

/// Log sink — components emit `LogEvent` here for structured logging
/// that surfaces through `engine_logs`.
pub const SINK_LOG: MailboxId = mailbox_id_from_name("aether.sink.log");

/// Handle store sink (ADR-0045). Publish / Drop / Resolve route
/// against the substrate-owned refcounted byte cache.
pub const SINK_HANDLE: MailboxId = mailbox_id_from_name("aether.sink.handle");
