//! Diagnostic and actor-monitoring kind vocabulary.

/// Diagnostic the hub emits back to an originating engine when mail
/// that engine bubbled up (ADR-0037) doesn't resolve at the hub
/// either. Lands on the engine's `aether.diagnostics` sink, which
/// re-warns locally so the unresolved address surfaces in that
/// engine's `engine_logs` rather than only in the hub's. Closes the
/// "typo diagnostics" follow-up from ADR-0037 (issue #185).
///
/// `recipient_mailbox_id` is the hashed mailbox id the originator
/// sent to — the id space is cross-process-stable (ADR-0029 /
/// ADR-0030 / issue #186) so agents can map it back to a name in
/// tooling. `kind_id` is the kind the original mail carried.
#[aether_data::kind(name = "aether.mail.unresolved", copy, eq)]
pub struct UnresolvedMail {
    pub recipient_mailbox_id: aether_data::MailboxId,
    pub kind_id: aether_data::KindId,
}

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
#[repr(C)]
#[aether_data::kind(name = "aether.actor.monitor_notice", pod, default, eq)]
pub struct MonitorNotice;
