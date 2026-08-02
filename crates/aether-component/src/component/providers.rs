//! The params provider registry (ADR-0170): the component host's map from a
//! requested kind to the function that reads it off load-time context.
//!
//! A component declares what it needs on its `Params` type; the host is the
//! container that supplies it. Every provider is a pure read of
//! [`LoadContext`] — no I/O, no mail, no clock — which is why the registry
//! holds bare `fn` pointers rather than boxed closures: a value a provider
//! could only obtain by doing work is not a load-time fact, and the signature
//! says so before any review does.
//!
//! Two rules the registry enforces rather than documents. Duplicate
//! registration is a boot error, so two composers cannot both claim a kind and
//! leave which one wins to declaration order. And a request with no provider
//! is a load error raised **before** instantiation, so a component never boots
//! with a fact silently absent.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_data::wire;
use aether_data::{Kind, KindId, ParamEntry};
use aether_kinds::{ParamRequirement, ReplicaIdentity};

use aether_substrate::mail::MailboxId;

/// The load-time facts every provider reads from (ADR-0170).
///
/// Everything here is known before the guest is instantiated, which is what
/// makes the providers pure: the instance's resolved name, the mailbox id it
/// was spawned into, and its position in a `replicas: N` fan-out.
#[derive(Debug, Clone, Copy)]
pub struct LoadContext<'a> {
    /// The name this instance registers under — the load's resolved name, so
    /// a `replicas: N` fan-out's `{base}-{index}` is what a provider sees.
    pub instance_name: &'a str,
    /// The instance's own lineage-folded mailbox id, assigned by the spawn
    /// that is constructing it.
    pub mailbox_id: MailboxId,
    /// Which instance of its fan-out this is. [`ReplicaIdentity::SOLE`] for an
    /// unreplicated load.
    pub replica: ReplicaIdentity,
}

/// A pure read of load-time context, producing one kind's wire bytes.
///
/// A bare `fn` pointer, not a boxed closure: a provider that needed captured
/// state would be reading something other than the load context, which is the
/// line ADR-0170 draws. A fact a chassis knows but the context does not
/// belongs on [`LoadContext`], not in a capture.
pub type ParamProvider = fn(&LoadContext<'_>) -> Vec<u8>;

/// Two registrations claimed the same kind. A boot error rather than a
/// last-one-wins overwrite: whichever provider the composition happened to
/// register second would otherwise silently decide what every component in the
/// fleet receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateParamProvider {
    /// The kind both registrations claimed.
    pub kind: KindId,
    /// The already-registered provider's kind name, for the boot message.
    pub kind_name: &'static str,
}

impl fmt::Display for DuplicateParamProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "duplicate params provider for kind {} ({}): a kind is provided by exactly one registration",
            self.kind_name, self.kind
        )
    }
}

impl Error for DuplicateParamProvider {}

/// Why a load's param requests could not be satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingParamProvider {
    /// The unprovided kind's name, as the component declared it.
    pub kind_name: String,
    /// The `Params` field that requested it.
    pub field: String,
}

impl fmt::Display for MissingParamProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "component requests param kind {:?} for field `{}`, which this chassis has no provider for \
             (ADR-0170: every request is required; register a provider or drop the field)",
            self.kind_name, self.field
        )
    }
}

impl Error for MissingParamProvider {}

/// The component host's kind-to-provider map (ADR-0170). Seeded with the
/// substrate's own facts by [`Self::with_substrate_facts`] and extended by a
/// chassis that knows more.
#[derive(Debug, Clone, Default)]
pub struct ParamProviderRegistry {
    providers: BTreeMap<KindId, (ParamProvider, &'static str)>,
}

impl ParamProviderRegistry {
    /// An empty registry — nothing is providable. Useful for a host that
    /// deliberately offers no facts (and therefore rejects every request);
    /// the ordinary construction is [`Self::with_substrate_facts`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The registry every chassis starts from: the facts the substrate itself
    /// knows about a load, independent of what the chassis composes.
    ///
    /// Today that is [`ReplicaIdentity`] — the motivating case, since
    /// replicated instances share one config by contract and so have no other
    /// principled source for their own index. `LoadContext`'s remaining fields
    /// are already carried, so seeding a further fact is one `register` call
    /// plus the kind it is delivered as.
    ///
    /// # Panics
    ///
    /// Never in practice: the seeding registers into a registry it just
    /// created, so the duplicate check cannot fire. The `expect` states that
    /// rather than silently discarding a `Result`.
    #[must_use]
    pub fn with_substrate_facts() -> Self {
        let mut registry = Self::default();
        registry
            .register::<ReplicaIdentity>(|ctx| ctx.replica.encode_into_bytes())
            .expect("a fresh registry has no prior claim on the substrate's own kinds");
        registry
    }

    /// Claim `K` for `provider`.
    ///
    /// # Errors
    ///
    /// [`DuplicateParamProvider`] when `K` is already claimed. Propagate it —
    /// a chassis that swallows this has two composers disagreeing about what a
    /// fact means.
    pub fn register<K: Kind>(&mut self, provider: ParamProvider) -> Result<(), DuplicateParamProvider> {
        if self.providers.contains_key(&K::ID) {
            return Err(DuplicateParamProvider { kind: K::ID, kind_name: K::NAME });
        }
        self.providers.insert(K::ID, (provider, K::NAME));
        Ok(())
    }

    /// Whether some provider claims `kind`.
    #[must_use]
    pub fn provides(&self, kind: KindId) -> bool {
        self.providers.contains_key(&kind)
    }

    /// Reject a load whose requests this registry cannot cover — the check the
    /// component host runs before it spawns anything, so a missing fact fails
    /// the load rather than reaching a half-injected `init`.
    ///
    /// # Errors
    ///
    /// [`MissingParamProvider`] naming the first unprovided request, in the
    /// component's own declaration order.
    pub fn validate(&self, requests: &[ParamRequirement]) -> Result<(), MissingParamProvider> {
        for request in requests {
            if !self.provides(request.id) {
                return Err(MissingParamProvider { kind_name: request.name.clone(), field: request.field.clone() });
            }
        }
        Ok(())
    }

    /// Build the bag for one load: one entry per request, in declaration
    /// order.
    ///
    /// # Errors
    ///
    /// [`MissingParamProvider`] on the first unprovided request. Callers
    /// validate at load time, so reaching this arm means the registry changed
    /// between validation and instantiation — it is a real failure, not a
    /// formality.
    pub fn provide(
        &self,
        requests: &[ParamRequirement],
        ctx: &LoadContext<'_>,
    ) -> Result<Vec<ParamEntry>, MissingParamProvider> {
        requests
            .iter()
            .map(|request| {
                let (provider, _) = self.providers.get(&request.id).ok_or_else(|| MissingParamProvider {
                    kind_name: request.name.clone(),
                    field: request.field.clone(),
                })?;
                Ok(ParamEntry { kind: request.id, bytes: provider(ctx) })
            })
            .collect()
    }

    /// The wire-encoded bag the FFI ships, or an empty vec when the component
    /// requested nothing — a no-request load spends no bytes saying so, which
    /// is what keeps it byte-identical to the pre-ADR-0170 path.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::provide`]'s missing-provider failure, and reports an
    /// encode failure as a plain message (the bag is a `Vec` of byte vectors,
    /// so this is unreachable short of an allocator failure).
    pub fn encode_bag(&self, requests: &[ParamRequirement], ctx: &LoadContext<'_>) -> Result<Vec<u8>, String> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let entries = self.provide(requests, ctx).map_err(|e| e.to_string())?;
        wire::to_vec(&entries).map_err(|e| format!("params bag encode failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement(kind_name: &str, id: KindId, field: &str) -> ParamRequirement {
        ParamRequirement { id, name: kind_name.to_owned(), field: field.to_owned() }
    }

    /// The bug: `register` written as a bare `BTreeMap::insert`, so a second
    /// claim on a kind silently replaces the first and the fleet's components
    /// receive whatever the composition happened to register last.
    #[test]
    fn second_claim_on_a_kind_is_an_error_not_an_overwrite() {
        let mut registry = ParamProviderRegistry::with_substrate_facts();

        let duplicate = registry
            .register::<ReplicaIdentity>(|_| Vec::new())
            .expect_err("a kind the substrate already provides cannot be re-registered");

        assert_eq!(duplicate.kind, <ReplicaIdentity as Kind>::ID);
        // The surviving provider is the original, not the intruder: a rejected
        // registration must leave the map untouched.
        let ctx = LoadContext {
            instance_name: "handler-2",
            mailbox_id: MailboxId(0x1234),
            replica: ReplicaIdentity { index: 2, count: 5 },
        };
        let entries = registry
            .provide(&[requirement(<ReplicaIdentity as Kind>::NAME, <ReplicaIdentity as Kind>::ID, "replica")], &ctx)
            .expect("the original provider is still registered");
        assert_eq!(ReplicaIdentity::decode_from_bytes(&entries[0].bytes), Some(ReplicaIdentity { index: 2, count: 5 }),);
    }

    /// The bug: validation that accepts an unprovided request — with a
    /// `contains_key` inverted, or the loop returning `Ok` on the first hit —
    /// letting the load reach instantiation and fail inside the guest (or,
    /// worse, boot with the field silently absent).
    #[test]
    fn a_request_with_no_provider_fails_validation_naming_kind_and_field() {
        let registry = ParamProviderRegistry::with_substrate_facts();
        let requests = vec![
            requirement(<ReplicaIdentity as Kind>::NAME, <ReplicaIdentity as Kind>::ID, "replica"),
            requirement("aether.test.unprovided", KindId(0xDEAD_BEEF), "unprovided"),
        ];

        let missing = registry.validate(&requests).expect_err("the second request has no provider");

        assert_eq!(missing.kind_name, "aether.test.unprovided");
        assert_eq!(missing.field, "unprovided");
    }

    /// The bug: a no-request load still encoding an empty `Vec<ParamEntry>`,
    /// which is four bytes rather than zero — enough to make every existing
    /// component take the params-bearing path and stop being a proof that the
    /// empty path still works.
    #[test]
    fn a_component_that_requests_nothing_ships_a_zero_byte_bag() {
        let registry = ParamProviderRegistry::with_substrate_facts();
        let ctx = LoadContext { instance_name: "probe", mailbox_id: MailboxId(0x1234), replica: ReplicaIdentity::SOLE };

        assert!(registry.encode_bag(&[], &ctx).expect("an empty request list always encodes").is_empty());
    }
}
