//! A mail bundle that crossed the boundary inside a payload, proven once.
//!
//! `DispatchTraced` and `CaptureFrame` carry a list of [`NamedMail`]s: each
//! item names its recipient as an [`ActorPath`](aether_data::ActorPath), an
//! address and nothing more. ADR-0230 section 3 makes the receiving engine
//! prove that address once and send only through the proof, so a bundle is
//! the same boundary inside a payload, and a wire `Call` is the same boundary
//! for one item: its recipient is an `ActorPath` the hosting engine proves on
//! arrival.
//! [`NativeCtx::accept_bundle`](crate::actor::native::NativeCtx::accept_bundle) proves every recipient
//! before any item moves, and
//! [`NativeCtx::accept_call`](crate::actor::native::NativeCtx::accept_call) proves a `Call`'s one.
//! Both hand back [`BoundaryMail`]s, which a holder can only deliver — through
//! [`NativeCtx::deliver_detached`](crate::actor::native::NativeCtx::deliver_detached)
//! or [`NativeCtx::deliver_forwarded`](crate::actor::native::NativeCtx::deliver_forwarded).
//! The proof never leaves the item and the item is never exportable, so a
//! boundary-derived reference cannot land in actor state as something a cap
//! sends other kinds through.

use aether_actor::ErasedActorRef;
use aether_data::ActorPath;
use aether_kinds::NamedMail;

use crate::mail::KindId;
use crate::mail::registry::Registry;

/// One bundle item whose recipient is proven: the proof, the kind the
/// boundary named, and the bytes the boundary encoded.
///
/// No public constructor, no accessor, no `Clone`, and no serde, wire, or
/// `Schema` impl: [`NativeCtx::accept_bundle`](crate::actor::native::NativeCtx::accept_bundle) and
/// [`NativeCtx::accept_call`](crate::actor::native::NativeCtx::accept_call) are the only ways to make one,
/// and delivering it is the only thing a holder can do with it.
#[derive(Debug)]
pub struct BoundaryMail {
    pub(crate) recipient: ErasedActorRef,
    pub(crate) kind: KindId,
    pub(crate) payload: Vec<u8>,
}

/// Prove every item in `bundle`, returning them only if all prove. On the
/// first refusal return a formatted error tagged with `label` (e.g.
/// `"capture bundle"`); the caller surfaces it as a `*Result::Err`, and no
/// item has moved.
///
/// Each recipient resolves through [`Registry::resolve_address`], not
/// `lookup`, so an ADR-0166 short-path recipient reports what actually went
/// wrong. `lookup` collapses every structured failure to `None`, which made an
/// *ambiguous* address — one whose hole matches several instanced child
/// namespaces — indistinguishable from an absent one, losing
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
        let recipient = prove(registry, &item.recipient)
            .map_err(|error| format!("recipient `{}` in {label}: {error}", item.recipient))?;
        let kind =
            registry.kind_id(&item.kind_name).ok_or_else(|| format!("unknown kind {:?} in {label}", item.kind_name))?;
        accepted.push(BoundaryMail { recipient, kind, payload: item.payload });
    }
    Ok(accepted)
}

/// Prove one wire `Call`'s recipient (ADR-0230 §3): the one-item form of
/// [`accept`], for a boundary that names its recipient by [`ActorPath`] and
/// its kind by id.
///
/// The path resolves and is proven exactly as a bundle item's is, so a short
/// path expands against this engine's declarations and an ambiguous one
/// carries its candidate spellings. The kind is taken as the caller gave it:
/// a wire `Call` carries no kind name to look up. On a refusal the error is
/// the registry's diagnostic alone, which the RPC server carries to the
/// caller as `RpcError::NotPresent`'s `detail` beside the path.
pub(crate) fn accept_call(
    registry: &Registry,
    recipient: &ActorPath,
    kind: KindId,
    payload: Vec<u8>,
) -> Result<BoundaryMail, String> {
    Ok(BoundaryMail { recipient: prove(registry, recipient)?, kind, payload })
}

/// Resolve `recipient` through [`Registry::resolve_address`] and prove the
/// answered position at once through [`Registry::resolve_live`]. The position
/// never leaves this function; each refusal is the registry's error text.
fn prove(registry: &Registry, recipient: &ActorPath) -> Result<ErasedActorRef, String> {
    let resolved = registry.resolve_address(recipient).map_err(|error| error.to_string())?;
    registry.resolve_live(resolved.mailbox_id).map_err(|error| error.to_string())
}

/// ADR-0166 §5 — the structured resolution diagnostic reaching the bundle
/// proof (issue 4125). The bundle path used to call `Registry::lookup`, which
/// collapses every `AddressResolutionError` except the path caps to `None` — so
/// an *ambiguous* short path reported "unknown recipient" with no
/// candidates and no indication that the address was ambiguous rather than
/// absent. The ambiguity is only reachable from real linked inventory, so the
/// fixtures below declare it: two instanced children beneath one root make a
/// hole under that root ambiguous by construction.
#[cfg(test)]
// The fixtures register their own canonical mailboxes: the fold of an expanded
// path is the reference value under test, not a sibling-cap address.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: moved test; fixtures fold their own canonical paths
mod tests {
    use aether_actor::Addressable;
    use aether_data::{ActorPath, Kind, mailbox_id_from_path};

    use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use crate::chassis::error::BootError;
    use crate::mail::registry::noop_handler;
    use crate::testing::boot_authority;

    use super::*;

    #[aether_data::kind(name = "test.bundle_diagnostics.poke", copy, default, eq)]
    struct Poke {
        value: u64,
    }

    /// The anchor root the two instanced children hang beneath.
    struct DiagnosticsRoot;

    #[aether_actor::actor(singleton, root)]
    impl NativeActor for DiagnosticsRoot {
        type Config = ();
        const NAMESPACE: &'static str = "test.bundle_diagnostics.root";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[allow(clippy::unused_self)] // aether-suppression-request: moved test fixture; handler ABI takes self
        #[handler::single]
        fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
    }

    /// First instanced child. On its own it would fill a hole beneath the
    /// root; paired with [`SecondChild`] it makes that hole ambiguous
    /// instead, which is the state under test.
    struct FirstChild;

    #[aether_actor::actor(instanced, child_of(DiagnosticsRoot))]
    impl NativeActor for FirstChild {
        type Config = ();
        const NAMESPACE: &'static str = "test.bundle_diagnostics.first";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[allow(clippy::unused_self)] // aether-suppression-request: moved test fixture; handler ABI takes self
        #[handler::single]
        fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
    }

    /// Second instanced child beneath the same root — the other half of the
    /// ambiguity.
    struct SecondChild;

    #[aether_actor::actor(instanced, child_of(DiagnosticsRoot))]
    impl NativeActor for SecondChild {
        type Config = ();
        const NAMESPACE: &'static str = "test.bundle_diagnostics.second";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[allow(clippy::unused_self)] // aether-suppression-request: moved test fixture; handler ABI takes self
        #[handler::single]
        fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
    }

    fn register(registry: &Registry, canonical: &str) {
        let mailbox_id = mailbox_id_from_path(canonical);
        registry
            .try_register_inbox_with_id(&boot_authority(), mailbox_id, canonical, noop_handler())
            .expect("canonical name is free");
    }

    fn bundle(recipient: &str) -> Vec<NamedMail> {
        vec![NamedMail {
            recipient: ActorPath::new(recipient).expect("fixture recipient is a well-formed actor path"),
            kind_name: <Poke as Kind>::NAME.to_owned(),
            payload: Poke { value: 1 }.encode_into_bytes(),
            count: 1,
        }]
    }

    /// The three outcomes the bundle path must tell apart. Before #4125 the
    /// middle one rendered identically to the last: `lookup` returned `None`
    /// either way, so the candidate spellings were dropped and the message
    /// claimed the recipient was unknown.
    #[test]
    fn bundle_resolution_distinguishes_ambiguous_from_absent_and_resolves_canonical() {
        let registry = Registry::new();
        registry.register_kind(&boot_authority(), <Poke as Kind>::NAME);
        let canonical = format!("{}/{}:one", DiagnosticsRoot::NAMESPACE, FirstChild::NAMESPACE);
        register(&registry, DiagnosticsRoot::NAMESPACE);
        register(&registry, &canonical);

        // Canonical input is unchanged — it never touched short-path expansion.
        let accepted = accept(&registry, bundle(&canonical), "test bundle").expect("canonical recipient resolves");
        assert_eq!(accepted.len(), 1);

        // Ambiguous: two instanced children are declared beneath the root, so a
        // hole cannot pick one. The error must say so and list both spellings
        // that would disambiguate it.
        let ambiguous = format!("{}/:one", DiagnosticsRoot::NAMESPACE);
        let error = accept(&registry, bundle(&ambiguous), "test bundle").expect_err("hole ambiguous");
        assert!(error.contains("ambiguous"), "the error names the ambiguity: {error}");
        assert!(error.contains(FirstChild::NAMESPACE), "the error lists the first candidate: {error}");
        assert!(error.contains(SecondChild::NAMESPACE), "the error lists the second candidate: {error}");

        // Absent still reports absence, explicitly and distinguishably.
        let absent = format!("{}/{}:missing", DiagnosticsRoot::NAMESPACE, FirstChild::NAMESPACE);
        let error = accept(&registry, bundle(&absent), "test bundle").expect_err("absent recipient");
        assert!(error.contains("no live mailbox"), "an absent recipient still reports absence: {error}");
        assert!(!error.contains("ambiguous"), "an absent recipient is not reported as ambiguous: {error}");
    }
}
