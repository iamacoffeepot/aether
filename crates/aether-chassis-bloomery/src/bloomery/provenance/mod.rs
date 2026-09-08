//! The fail-closed instruction-provenance gate every model dispatch passes
//! (ADR-0149 §The value vocabulary, ADR-0214).
//!
//! ADR-0149 requires a validated prompt manifest before a model is invoked, and
//! [`assemble_manifest`] is that validator. Until this module it had no
//! production caller: the dispatch path recorded an order and submitted it, and
//! nothing asked where the lane's instructions came from (#5589). This is that
//! caller, and it sits at `dispatch_and_record` — the one host step every model
//! lane goes through — so a new dispatch site cannot acquire a model lane
//! without passing here.
//!
//! # What grounds an instruction slot
//!
//! ADR-0214 makes model-process instructions explicit configuration: a
//! content-addressed [`ModelProcessInstructions`] bundle sealed into the bloom's
//! `ConfigRegistry` (ADR-0174) and authorized by the host operator. That is the
//! *versioned policy artifact* arm of
//! ADR-0149's closure, so the gate resolves the pin, verifies the stored bytes
//! re-address to it, checks the bundle is complete, checks the host authorized
//! that exact digest, and only then offers it to the assembler as the manifest's
//! instruction slot.
//!
//! The dispatched work order rides as a **context** slot and the subject as a
//! **reference** slot — never as instructions. That is ADR-0214 §Process policy
//! does not authorize arbitrary task text made structural: an authorized bundle
//! establishes the process rules, and attaching it to a task cannot lend the task
//! policy authority.
//!
//! Every failure refuses the dispatch. Nothing falls back to instructions from
//! the checkout, and a refusal is journaled as the host fault it is rather than
//! logged and dropped — see [`refusal_fault`].

mod policy;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery::{
    Admit, AuthorityDoor, AuthorizedSigner, ClosureViolation, ConfigResolveError, ConfigScopes, Digest,
    Ed25519KeyProvider, Event, Evidence, EvidenceKind, Fact, IdempotencyKey, KeyId, ModelProcessInstructions,
    ModelProcessInstructionsError, PromptManifest, ProvenanceIndex, SCOPE_FILL_COMMAND, Slot, SlotRole, StageId,
    Statement, Topic, assemble_manifest, config_address, decode_config, is_model_lane,
};
#[cfg(any(test, feature = "testing"))]
use aether_bloomery::{ConfigKind, ConfigRegistry};
use aether_data::Kind;
use aether_data::wire::to_vec;

pub use policy::ProcessPolicy;

use crate::bloomery::intake::DispatchRecord;
use crate::bloomery::outbox::TopicOutbox;
use crate::store::StoreBackend;

/// Whether this dispatch must present validated instruction provenance before it
/// may reach a worker.
///
/// Every model lane except the pre-bloom scoping run. A scoping run is dispatched
/// before any bloom exists, so it carries no sealed registry a pin could live in
/// (ADR-0214 §Resolve defaults before sealing asks for a pin on the durable scope
/// run itself, which is a persisted identity this gate does not yet have). Gating
/// it against a registry that cannot hold a pin would refuse every scoping run
/// rather than enforce anything, so it is excluded here, by name, until that pin
/// exists.
#[must_use]
pub fn gated(command: &str) -> bool {
    is_model_lane(command) && command != SCOPE_FILL_COMMAND
}

/// Why a dispatch was refused before it could reach a model.
///
/// Every variant names what was pinned and where the trail went cold, so the
/// refusal reads without a debugger. Only [`Self::Store`] is transient — the
/// rest are statements about immutable content, an immutable authorization
/// set, or this host's own boot configuration, and will answer identically on
/// every retry.
#[derive(Debug)]
pub enum ProvenanceRefusal {
    /// The dispatch's sealed configuration pins no instruction bundle. An
    /// ADR-0214 bloom pins one before sealing; one that does not cannot start a
    /// model attempt.
    Unpinned,
    /// A bundle is pinned and its content cannot be produced — absent,
    /// mis-filed, or undecodable.
    Unresolvable(ConfigResolveError),
    /// The stored bytes do not re-address to the digest that was pinned, so the
    /// content behind the pin is not the content the pin names.
    ContentMismatch {
        /// The address the registry sealed.
        pinned: Digest,
        /// The address the stored bytes actually hash to.
        stored: Digest,
    },
    /// The pinned bundle leaves an instruction field empty.
    Incomplete {
        /// The bundle that was pinned.
        bundle: Digest,
        /// Which field is empty.
        error: ModelProcessInstructionsError,
    },
    /// The pinned bundle is complete and resolvable, and the host operator never
    /// authorized it as process policy.
    Unauthorized {
        /// The bundle that was pinned.
        bundle: Digest,
    },
    /// Manifest assembly refused a slot's derivation closure.
    Closure(ClosureViolation),
    /// This host does not dispatch the bloom-level reader (ADR-0216 §4).
    ///
    /// Not produced by [`admit_model_dispatch`]: the study drain refuses before
    /// the gate is reached, because there is nothing to resolve for a lane that
    /// is not going to run. It travels through [`journal_refusal`] anyway
    /// because a read this host declines to spend is exactly the shape every
    /// other refusal here is — a dispatch that will not reach a model, owed a
    /// journal entry rather than a dropped log line, so the bloom lands with a
    /// study that is legibly missing.
    ReaderDisabled,
    /// The store faulted while resolving the pin. Transient: it says nothing
    /// about the content.
    Store(rusqlite::Error),
}

impl ProvenanceRefusal {
    /// Whether this refusal will answer identically on every retry, so the
    /// caller parks the dispatch rather than re-driving it forever.
    #[must_use]
    pub const fn is_permanent(&self) -> bool {
        !matches!(self, Self::Store(_))
    }
}

impl fmt::Display for ProvenanceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unpinned => write!(
                f,
                "no `{}` bundle is pinned; an unpinned bloom cannot start a model attempt (ADR-0214)",
                ModelProcessInstructions::NAME
            ),
            Self::Unresolvable(error) => write!(f, "pinned instruction bundle is unresolvable: {error}"),
            Self::ContentMismatch { pinned, stored } => write!(
                f,
                "instruction bundle pinned at {} is stored as content addressing {}",
                pinned.to_hex(),
                stored.to_hex()
            ),
            Self::Incomplete { bundle, error } => match error {
                ModelProcessInstructionsError::EmptyField(field) => {
                    write!(f, "instruction bundle {} leaves `{field}` empty", bundle.to_hex())
                }
            },
            Self::Unauthorized { bundle } => write!(
                f,
                "instruction bundle {} is not authorized by this host as model-process policy",
                bundle.to_hex()
            ),
            Self::Closure(violation) => write!(f, "prompt manifest refused: {violation:?}"),
            Self::ReaderDisabled => write!(
                f,
                "this host does not dispatch the bloom-level reader (ADR-0216 §4); the study is missing by \
                 configuration, not by failure"
            ),
            Self::Store(error) => write!(f, "instruction-bundle lookup failed: {error}"),
        }
    }
}

impl Error for ProvenanceRefusal {}

/// Resolve, verify, authorize, and assemble the prompt manifest one model
/// dispatch runs under, or refuse the dispatch.
///
/// The order of the checks is the order they can be answered in, and each is a
/// refusal rather than a fall-through. Content identity is re-derived from the
/// stored bytes rather than trusted, because a row filed at an address it does
/// not hash to would otherwise let one bundle's authorization stand for another
/// bundle's text.
///
/// # Errors
///
/// [`ProvenanceRefusal`], naming the pin and the step that refused.
pub fn admit_model_dispatch(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
) -> Result<PromptManifest, ProvenanceRefusal> {
    // The record's registry is already the member's layered over the bloom's, so
    // a bloom-wide lookup over it resolves the member's pin when it seals one and
    // the bloom's otherwise. A member cannot *widen* what the host authorized —
    // whichever pin wins still has to be in the policy set below.
    let pinned = ConfigScopes::bloom_wide(&record.configs)
        .address::<ModelProcessInstructions>()
        .ok_or(ProvenanceRefusal::Unpinned)?;
    let (kind, bytes, schema_digest) = store
        .lookup_config(pinned.as_bytes())
        .map_err(ProvenanceRefusal::Store)?
        .ok_or(ProvenanceRefusal::Unresolvable(ConfigResolveError::Missing { kind: ModelProcessInstructions::NAME }))?;

    let stored = config_address(&kind, &bytes);
    if stored != pinned {
        return Err(ProvenanceRefusal::ContentMismatch { pinned, stored });
    }

    decode_config::<ModelProcessInstructions>(&kind, &bytes, schema_digest.as_deref())
        .map_err(ProvenanceRefusal::Unresolvable)?
        .validate()
        .map_err(|error| ProvenanceRefusal::Incomplete { bundle: pinned, error })?;

    if !store.instructions_authorized(pinned.as_bytes()).map_err(ProvenanceRefusal::Store)? {
        return Err(ProvenanceRefusal::Unauthorized { bundle: pinned });
    }

    assemble_manifest(slots(record, pinned), &AuthorizedPolicy { bundle: pinned }, &no_signers())
        .map_err(ProvenanceRefusal::Closure)
}

/// The slots this dispatch's prompt is assembled from, in prompt order.
///
/// One instruction slot — the authorized bundle — and nothing else. The work
/// order the reducer sealed and the subject the evidence binds to are material
/// the model reads, not commands it takes its process from, so they ride as
/// context and reference and carry no grounding requirement of their own.
fn slots(record: &DispatchRecord, bundle: Digest) -> Vec<Slot> {
    let mut slots = vec![Slot { artifact: bundle, role: SlotRole::Instruction, parent_closure: Vec::new() }];

    if let Some(task) = record.transformation.description.as_deref() {
        slots.push(Slot {
            artifact: Digest::of_wire_bytes(task.as_bytes()),
            role: SlotRole::Context,
            parent_closure: Vec::new(),
        });
    }
    slots.push(Slot { artifact: record.displayed_digest, role: SlotRole::Reference, parent_closure: Vec::new() });
    slots
}

/// The verification port manifest assembly calls through here.
///
/// The real ed25519 verifier over an empty allowlist. Statement-grounded
/// instruction slots need a persisted statement store this host does not have
/// yet, so [`AuthorizedPolicy`] answers no statements at all and this provider is
/// never consulted; giving it an empty allowlist rather than the always-valid
/// stub is what keeps that true if a statement ever does reach the walk.
fn no_signers() -> Ed25519KeyProvider {
    Ed25519KeyProvider::new(BTreeMap::<KeyId, AuthorizedSigner>::new())
}

/// The production provenance index: one authorized instruction bundle, and
/// nothing else grounds.
///
/// ADR-0149's closure admits either a versioned policy artifact or an
/// author-signed statement. ADR-0214 supplies the first — the operator-authorized
/// bundle — and this index reports exactly that one artifact as policy. Every
/// other digest is unknown to it, which is what makes a candidate-supplied
/// instruction slot refuse: an artifact the index cannot answer for has no
/// derivation edge to walk and no ground to reach.
struct AuthorizedPolicy {
    bundle: Digest,
}

impl ProvenanceIndex for AuthorizedPolicy {
    fn statement(&self, _digest: &Digest) -> Option<&Statement> {
        None
    }

    fn is_versioned_policy(&self, digest: &Digest) -> bool {
        *digest == self.bundle
    }

    fn parents(&self, digest: &Digest) -> Option<Vec<Digest>> {
        (*digest == self.bundle).then(Vec::new)
    }

    fn authority_binding(&self, _digest: &Digest) -> Option<(AuthorityDoor, Digest)> {
        None
    }
}

/// The journal entry a refused dispatch becomes: the host reporting that it
/// could not run this stage at all.
///
/// The same fact a lane that failed to launch produces (ADR-0195) — the cursor
/// does not move, the candidate does not move, no verdict is recorded about the
/// member's work, and the member spends a machinery roll. That is the honest
/// account of a refusal: the process the host would have judged the candidate
/// with was not authorized, which says nothing about the candidate. At the sealed
/// machinery ceiling the member wedges and names machinery, so an unauthorized
/// process stops the line visibly instead of stalling it silently.
///
/// The idempotency key is the order's nonce, which is a pure function of the
/// outbox sequence: a re-drive of the same entry journals once, and each fresh
/// redispatch the reducer decides journals its own roll.
#[must_use]
pub fn refusal_fault(record: &DispatchRecord, refusal: &ProvenanceRefusal) -> Event {
    let evidence = Evidence {
        subject: record.displayed_digest,
        kind: EvidenceKind::ExecutorFault,
        detail: Digest::of_wire_bytes(refusal.to_string().as_bytes()),
    };
    let key =
        IdempotencyKey(format!("aether.bloomery.provenance_refusal:{}:{}", record.bloom.0.to_hex(), record.nonce.0));
    let fact = if record.stage == StageId::AggregateReview {
        Fact::AggregateReviewExecutorFault { bloom: record.bloom, evidence }
    } else if record.stage == StageId::Study {
        // The bloom-level reader (ADR-0216) has no member axis and no bloom
        // left to stop: it is dispatched at the landing, so a refusal here is
        // the study going missing and nothing else. Routed as the reader's own
        // fact rather than the member one, whose empty workpiece would name no
        // member of a bloom that has already released them all.
        Fact::StudyCompleted { bloom: record.bloom, passed: false, evidence }
    } else {
        Fact::MemberExecutorFault {
            bloom: record.bloom,
            workpiece: record.workpiece.clone(),
            stage: record.stage,
            evidence,
        }
    };

    Event { idempotency_key: key, fact }
}

/// Park a permanent refusal on [`Topic::RefusedDispatch`] so the reactor's next
/// tick admits it.
///
/// Durable rather than an in-process hand-off, for the same reason every other
/// outbox row is: the refusal is the only account of why a member stopped
/// dispatching, and one that lived in the drain's return value would evaporate on
/// a restart, leaving a parked member and a silent journal. A transient store
/// fault is not journaled at all — the dispatch re-drives, and a refusal that has
/// not been decided yet must not be recorded as though it had.
///
/// Best-effort on the write itself: a store that cannot take the row cannot take
/// it now either, and failing the refusal would turn "this dispatch is not
/// authorized" into "this dispatch is retried forever".
pub fn journal_refusal(store: &mut dyn StoreBackend, record: &DispatchRecord, refusal: &ProvenanceRefusal) {
    if !refusal.is_permanent() {
        return;
    }
    let Ok(event) = to_vec(&refusal_fault(record, refusal)).inspect_err(|error| {
        tracing::error!(
            target: "aether_chassis_bloomery::provenance",
            nonce = %record.nonce.0,
            %error,
            "provenance refusal did not encode; the dispatch is still refused but nothing is journaled",
        );
    }) else {
        return;
    };
    if let Err(error) = store.enqueue_topic(Topic::RefusedDispatch, &event, None) {
        tracing::error!(
            target: "aether_chassis_bloomery::provenance",
            nonce = %record.nonce.0,
            %error,
            "provenance refusal could not be parked for admission; the dispatch is still refused",
        );
    }
}

/// A complete instruction bundle for scenarios and tests: every field filled
/// with a line naming itself, so [`ModelProcessInstructions::validate`] passes
/// and a scenario asserting on a prompt can tell one field from another.
///
/// Not a seed for production. ADR-0214 §Migration wants the first real bundle
/// imported from the existing instruction files under an explicit operator
/// authorization, and this text is neither those files nor that authorization.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn reference_instructions() -> ModelProcessInstructions {
    let field = |name: &str| format!("Reference {name} instructions for scenarios.");
    ModelProcessInstructions {
        conventions: field("conventions"),
        construct: field("construct"),
        review: field("review"),
        scope: field("scope"),
        subject_unspecified: field("subject-unspecified"),
        subject_at_commit: field("subject-at-commit"),
        seeded_state: field("seeded-state"),
        construct_lint_repair: field("construct-lint-repair"),
        review_candidate_working_tree: field("review-candidate-working-tree"),
        review_candidate_committed: field("review-candidate-committed"),
        review_composition_contract: field("review-composition-contract"),
        scope_emission: field("scope-emission"),
        aggregate_full_pass: field("aggregate-full-pass"),
        aggregate_delta_confirm: field("aggregate-delta-confirm"),
        attribute_findings: field("attribute-findings"),
        fold_conflict_contract: field("fold-conflict-contract"),
        composition_refine_order: field("composition-refine-order"),
        retrospect: field("retrospect"),
        retrospect_finding_contract: field("retrospect-finding-contract"),
    }
}

/// Record `bundle`'s content as configuration and hand back a registry pinning
/// it — the authoring half, without the authorization.
///
/// The two halves are separate here because they are separate in ADR-0214:
/// storing a bundle's bytes says what it is, and only the host's own policy says
/// it may serve as process policy. A caller that boots a real coordinator writes
/// the content here and states the authorization through the coordinator's
/// configuration, which is the path a deployment takes.
///
/// # Panics
/// The store could not take the row.
#[cfg(any(test, feature = "testing"))]
pub fn pin_instructions(store: &mut dyn StoreBackend, bundle: &ModelProcessInstructions) -> ConfigRegistry {
    let address = bundle.address();
    let bytes = to_vec(bundle).expect("an instruction bundle encodes");
    store.record_config(address.as_bytes(), ModelProcessInstructions::NAME, &bytes).expect("the bundle records");

    let mut registry = ConfigRegistry::default();
    registry.insert::<ModelProcessInstructions>(address);
    registry
}

/// [`pin_instructions`] plus the host authorization, for a test that drives the
/// dispatch path directly rather than booting a coordinator that would seed the
/// authorization from its own configuration.
///
/// # Panics
/// The store could not take the rows.
#[cfg(any(test, feature = "testing"))]
pub fn authorize_instructions(store: &mut dyn StoreBackend, bundle: &ModelProcessInstructions) -> ConfigRegistry {
    let registry = pin_instructions(store, bundle);
    let authorized = registry.entries().map(|(_, address)| address.as_bytes().to_vec()).collect::<Vec<_>>();
    store.set_authorized_instructions(&authorized).expect("the authorization records");

    registry
}

/// Drain the parked refusals into the [`Admit`]s the reactor mails to the control
/// core, acking the batch it hands back.
///
/// Acked as it drains, like every other fire-and-forget admit path: the mail push
/// is in-process and the reducer dedups on the event's idempotency key, so a
/// redelivered row would journal nothing new while an unacked one would re-admit
/// on every tick forever.
pub fn drain_refusals(store: &mut dyn StoreBackend) -> rusqlite::Result<Vec<Admit>> {
    let entries = store.drain_topic(Topic::RefusedDispatch)?;
    let Some(through) = entries.last().map(|entry| entry.sequence) else {
        return Ok(Vec::new());
    };
    let admits = entries.into_iter().map(|entry| Admit { event: entry.payload }).collect();
    store.ack_topic(Topic::RefusedDispatch, through)?;

    Ok(admits)
}
