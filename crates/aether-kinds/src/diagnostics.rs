//! Diagnostic and actor-monitoring kind vocabulary.

/// Issue 607 Phase 4b (ADR-0079): framework-emitted close
/// notification. Sent to every monitor a closing actor accumulated via
/// `NativeCtx::monitor` — the substrate drains `monitors_of[target]`
/// after the target's `unwire` runs, fires one `MonitorNotice` per
/// watcher, and only then flips the target's slot from `Live` to
/// `Dead`.
///
/// The notice carries no fields: the host stamps the departed actor as
/// the envelope sender, so the watcher's handler reads it as a proven
/// reference from `ctx.sender()` and matches it against the references
/// it holds (ADR-0230). An inline child departs under its own alias
/// (ADR-0114 §4), the identity its sends stamp, so the notice's sender
/// is the alias rather than the host.
///
/// Engine-only mail (ADR-0233): the registry's `notify_departure` pushes it
/// from host code through the mailer, and no actor may send it.
#[repr(C)]
#[aether_data::kind(name = "aether.actor.monitor_notice", pod, default, eq, engine_only)]
pub struct MonitorNotice;
