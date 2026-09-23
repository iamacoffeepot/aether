//! A mail bundle that crossed the boundary inside a payload, proven once.
//!
//! `DispatchTraced` and `CaptureFrame` carry a list of [`NamedMail`]s: each
//! item names its recipient as an [`ActorPath`](aether_data::ActorPath), an
//! address and nothing more. ADR-0230 section 3 makes the receiving engine
//! prove that address once and send only through the proof, so a bundle is
//! the same boundary inside a payload. [`accept`] proves every recipient
//! before any item moves and hands back [`BoundaryMail`]s, which a holder can
//! only deliver — through
//! [`NativeCtx::deliver_detached`](crate::actor::native::NativeCtx::deliver_detached)
//! or [`NativeCtx::deliver_forwarded`](crate::actor::native::NativeCtx::deliver_forwarded).
//! The proof never leaves the item and the item is never exportable, so a
//! boundary-derived reference cannot land in actor state as something a cap
//! sends other kinds through.

use aether_actor::AnyActorRef;
use aether_kinds::NamedMail;

use crate::mail::KindId;
use crate::mail::registry::Registry;

/// One bundle item whose recipient is proven: the proof, the kind the
/// boundary named, and the bytes the boundary encoded.
///
/// No public constructor, no accessor, no `Clone`, and no serde, wire, or
/// `Schema` impl: [`accept`] is the only way to make one, and delivering it is
/// the only thing a holder can do with it.
#[derive(Debug)]
pub struct BoundaryMail {
    pub(crate) recipient: AnyActorRef,
    pub(crate) kind: KindId,
    pub(crate) payload: Vec<u8>,
}

/// Prove every item in `bundle`, returning them only if all prove. On the
/// first refusal return a formatted error tagged with `label` (e.g.
/// `"capture bundle"`); the caller surfaces it as a `*Result::Err`, and no
/// item has moved.
///
/// Each recipient resolves through [`Registry::resolve_address`], not
/// `lookup`, so an ADR-0166 abbreviated recipient reports what actually went
/// wrong. `lookup` collapses every structured failure to `None`, which made an
/// *ambiguous* address — one whose bare discriminator matches several
/// instanced child namespaces — indistinguishable from an absent one, losing
/// the candidate spellings ADR-0166 §5 specifies (issue 4125). This is the
/// path `send_mail_traced` and `capture_frame(mails=…)` take, so that
/// diagnostic is what an operator or agent sees.
///
/// The position `resolve_address` answers is proven at once through
/// [`Registry::resolve_live`] and never leaves this function; a recipient
/// whose birth is still `Starting` refuses like an absent one.
pub(crate) fn accept(registry: &Registry, bundle: Vec<NamedMail>, label: &str) -> Result<Vec<BoundaryMail>, String> {
    let mut accepted = Vec::with_capacity(bundle.len());
    for item in bundle {
        let recipient = registry
            .resolve_address(&item.recipient)
            .map_err(|error| error.to_string())
            .and_then(|resolved| registry.resolve_live(resolved.mailbox_id).map_err(|error| error.to_string()))
            .map_err(|error| format!("recipient `{}` in {label}: {error}", item.recipient))?;
        let kind =
            registry.kind_id(&item.kind_name).ok_or_else(|| format!("unknown kind {:?} in {label}", item.kind_name))?;
        accepted.push(BoundaryMail { recipient, kind, payload: item.payload });
    }
    Ok(accepted)
}
