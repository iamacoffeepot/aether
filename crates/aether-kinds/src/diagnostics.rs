//! Diagnostic and actor-monitoring kind vocabulary.

use alloc::string::String;

use aether_data::KindId;

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

/// The engine's empty watch context (ADR-0079 §8): what a component passes
/// to `ctx.watch` for a watched type whose departure handler takes no context
/// parameter, as in `ctx.watch(camera, NoContext)`.
///
/// A watch always stores a context, so a handler with nothing to note leaves
/// its fourth parameter out and its context kind is this one. No author
/// declares a kind to say nothing.
#[repr(C)]
#[aether_data::kind(name = "aether.actor.no_context", pod, default, eq)]
pub struct NoContext;

/// Host-generated notice that a request's recipient refused its payload at
/// decode. The refusing actor answers it to the request's reply target in
/// place of the reply it could not produce, so it joins the request's chain
/// and is handled before that chain's `Settled`.
///
/// Only a reply target that opts in hears it: one whose published contract
/// carries a row for this kind. Any other sender hears nothing, because an
/// in-engine sender is typed code and a refusal it causes is a codec bug the
/// refuser's log records. The RPC server declares the row, since a wire
/// payload is untrusted and its caller cannot read actor logs.
///
/// The refuser is the notice's sender, read as a proven reference from
/// `ctx.sender()`, so the notice carries no address. `kind` is the refused
/// payload's kind and `error` the decode error's text.
///
/// Engine-only mail (ADR-0233): the native decode path answers it from host
/// code, and no actor may send it.
#[aether_data::kind(name = "aether.mail.decode_refused", eq, engine_only)]
pub struct DecodeRefused {
    pub kind: KindId,
    pub error: String,
}
