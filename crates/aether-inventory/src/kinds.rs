//! The `aether.inventory` request / reply vocabulary — the cap's
//! caller-facing kinds (ADR-0088 §6, ADR-0091, ADR-0109 §5), owned here
//! per the `capability-anatomy.md` kind-ownership rule.
//!
//! The `aether.inventory` mailbox serves the per-build reverse-lookup
//! inventory over mail so an out-of-process observer (the MCP harness)
//! reads the running substrate's *own* state instead of a drift-prone
//! compiled-in copy. Six request kinds:
//!
//! - [`Manifest`] → the compile-time manifest: every declared
//!   `NameEntry` + every instanced-family `TemplateEntry`. Templates keep
//!   their *family shape* (the client expands a `Bounded` range /
//!   `Declared` domain itself); the manifest does NOT flatten to a
//!   hash → name map (ADR-0088 §6).
//! - [`Resolve`] → per-id `Option<String>`, for ids the client can't
//!   compute from the manifest alone (ADR-0088 §5).
//! - [`ResolveAddress`] → the live mailbox id and canonical path for a
//!   canonical or ADR-0166 short actor address.
//! - [`ListKinds`] → the engine's live kind vocabulary (ADR-0091).
//! - [`ListHandlers`] → the native handler manifest (ADR-0109 §5).
//! - [`ListMemory`] → what the engine holds, by owner.
//!
//! The link-time `aether_data::name_inventory::{NameEntry, TemplateEntry,
//! ParamKind}` are `&'static` (not wire types), so the shapes here are
//! owned, schema-hashed mirrors. `domain` rides as raw bytes (the
//! byte-domain prefix an id is hashed under, e.g. `MAILBOX_DOMAIN` /
//! `THREAD_DOMAIN`) so the client recomputes hashes exactly without
//! depending on the substrate's domain consts.
//!
//! [`KindDescriptorWire`] stays in `aether-kinds`: `aether-fleet` uses it
//! for component config descriptors, independent of this cap, so it is
//! shared vocabulary rather than inventory-owned.

use aether_kinds::KindDescriptorWire;
use serde::{Deserialize, Serialize};

/// How a [`TemplateEntryWire`]'s single `{…}` hole is filled — the
/// wire mirror of `aether_data::name_inventory::ParamKind` (ADR-0088
/// §4). The variants preserve the family shape so the client can
/// expand / prehash a `Bounded` range or `Declared` domain locally
/// the same way the substrate's static reverse map does at boot.
#[aether_data::kind(name = "aether.inventory.param_kind")]
pub enum ParamKindWire {
    /// Finite inclusive integer range (`aether-worker-{0..=255}`).
    /// The client enumerates `lo..=hi`, substitutes each value into
    /// the template, and hashes the result for an exact reverse.
    Bounded { lo: u64, hi: u64 },
    /// The hole ranges over every [`NameEntryWire`] whose `domain`
    /// equals `domain` (`aether-root-{NAMESPACE}` over the declared
    /// mailbox namespaces).
    Declared {
        #[serde(with = "aether_data::bytes")]
        domain: Vec<u8>,
    },
    /// Instances are minted at runtime from an unbounded parameter
    /// (`aether-instanced-{full_name}`). The template declares only
    /// the family's existence + shape; individual instances reverse
    /// via `aether.inventory.resolve`, not local expansion.
    Dynamic,
}

/// A declared name on the wire — the mirror of
/// `aether_data::name_inventory::NameEntry` (ADR-0088 §3). `domain`
/// is the byte-domain prefix the name is hashed under; `name` is the
/// declared name (`"aether.fs"`). The client rehashes `name` under
/// `domain` to recover the id space exactly.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NameEntryWire {
    #[serde(with = "aether_data::bytes")]
    pub domain: Vec<u8>,
    pub name: String,
}

/// A name template for an instanced family on the wire — the mirror
/// of `aether_data::name_inventory::TemplateEntry` (ADR-0088 §4).
/// `template` carries one `{…}` hole; [`ParamKindWire`] (the shape
/// axis) says how it is filled. Preserving the template (rather than
/// its expansion) keeps the family shape so the client can declare
/// "ids in this family exist and look like *this*" even for `Dynamic`
/// families it cannot enumerate.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub struct TemplateEntryWire {
    #[serde(with = "aether_data::bytes")]
    pub domain: Vec<u8>,
    pub template: String,
    pub param: ParamKindWire,
}

/// `aether.inventory.manifest` — request the running substrate's
/// compile-time reverse-lookup manifest (ADR-0088 §6). Empty payload;
/// the request *is* the signal. Mailed to the `"aether.inventory"`
/// mailbox; reply: [`ManifestResult`].
#[aether_data::kind(name = "aether.inventory.manifest")]
pub struct Manifest {}

/// Reply to [`Manifest`] (ADR-0088 §6). Carries every link-time
/// [`NameEntryWire`] (declared names: chassis mailbox namespaces +
/// kinds + transforms) and every [`TemplateEntryWire`] (instanced
/// families, `Bounded`/`Declared`/`Dynamic`). The client folds
/// `names` into a hash → name map and expands `Bounded`/`Declared`
/// templates locally; `Dynamic` templates resolve per-id via
/// [`Resolve`]. This is the *authoritative, per-build* inventory —
/// the served form is always the running substrate's own.
///
/// No `Err` arm: the reply reads a process-global link-time table, so
/// there is no failure mode to report.
#[aether_data::kind(name = "aether.inventory.manifest_result")]
pub struct ManifestResult {
    pub names: Vec<NameEntryWire>,
    pub templates: Vec<TemplateEntryWire>,
}

/// `aether.inventory.resolve` — request per-id reverse lookup
/// (ADR-0088 §5/§6). `ids` are ADR-0064 tagged-id strings
/// (`mbx-…` / `knd-…` / `thr-…` / `trn-…`) — the same wire form the
/// MCP surface carries elsewhere. Used on a *local miss*: the client
/// resolves statics + expandable templates from the manifest itself,
/// then asks the substrate only for dynamic-instance ids it can't
/// compute. Mailed to the `"aether.inventory"` mailbox; reply:
/// [`ResolveResult`].
#[aether_data::kind(name = "aether.inventory.resolve")]
pub struct Resolve {
    pub ids: Vec<String>,
}

/// One id → name pairing in a [`ResolveResult`] (ADR-0088 §6). `id`
/// echoes the request's tagged-id string so the caller correlates
/// without relying on positional order; `name` is the resolved origin
/// name, or `None` on a full miss (the id wasn't in the static map,
/// any prehashed template, or the runtime registry — the caller falls
/// back to rendering the tagged-id string per ADR-0064, exactly what
/// it showed before the inventory existed). Per the explicit-nulls
/// convention every entry addresses its `name` Option directly.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResolvedName {
    pub id: String,
    pub name: Option<String>,
}

/// Reply to [`Resolve`] (ADR-0088 §6). One [`ResolvedName`] per
/// requested id, in request order (and each echoing its `id` so the
/// caller can correlate without depending on order). An id that fails
/// to parse as a tagged-id string is reported as `name: None` rather
/// than aborting the batch — one bad id doesn't sink its siblings.
///
/// No `Err` arm: a miss is `name: None` per entry, so the batch itself
/// has no failure mode to report.
#[aether_data::kind(name = "aether.inventory.resolve_result")]
pub struct ResolveResult {
    pub resolved: Vec<ResolvedName>,
}

/// `aether.inventory.resolve_address` — resolve one canonical or ADR-0166
/// short actor address inside the selected engine. The engine registry
/// owns short-path expansion, canonical validation, and liveness, and it
/// answers the canonical path, never a mailbox position; external clients
/// must not fold the supplied string themselves.
#[aether_data::kind(name = "aether.inventory.resolve_address")]
pub struct ResolveAddress {
    pub address: String,
}

/// Reply to [`ResolveAddress`]. The success arm carries the live actor's
/// canonical path, so a caller displays and caches capabilities under the
/// registry's real identity; no mailbox position leaves the engine. The failure arm
/// intentionally carries only the registry's human-readable diagnostic rather
/// than duplicating the substrate's internal address-resolution error enum.
/// It exists — where the sibling [`ListKindsResult`] over the same live
/// `Registry` has none — because this is a lookup, and a lookup can miss.
#[aether_data::kind(name = "aether.inventory.resolve_address_result", eq)]
pub enum ResolveAddressResult {
    Ok { canonical_path: String },
    Err { error: String },
}

/// `aether.inventory.kinds` — request the running substrate's
/// authoritative kind vocabulary (ADR-0091): every
/// [`KindId`](aether_data::KindId) the engine's `Registry`
/// currently holds, with its full
/// [`SchemaType`](aether_data::SchemaType). Empty payload; the
/// request *is* the signal. Mailed to the `"aether.inventory"`
/// mailbox; reply: [`ListKindsResult`].
///
/// The MCP harness uses this to refresh its per-engine encode-
/// cache after a `load_component` registers a component's own
/// kinds — the substrate's `Registry` is the single source of
/// truth, projected onto the wire by the inventory cap.
#[aether_data::kind(name = "aether.inventory.kinds")]
pub struct ListKinds {}

/// Reply to [`ListKinds`] (ADR-0091). One [`KindDescriptorWire`] per
/// kind currently registered in the substrate's `Registry`, sorted
/// by name (the registry's `list_kind_descriptors` ordering). The
/// harness folds this into its per-engine encode cache; component-
/// defined kinds (loaded via `aether.component.load`) show up here
/// alongside the substrate's static vocabulary the moment the load
/// returns, no separate notification.
///
/// No `Err` arm, where the sibling [`ResolveAddressResult`] has one over
/// the same live `Registry`: this is an enumeration, which cannot miss —
/// an empty vocabulary is an empty list. A lookup can miss, so
/// `ResolveAddress` carries the failure arm and `ListKinds` does not.
#[aether_data::kind(name = "aether.inventory.kinds_result")]
pub struct ListKindsResult {
    pub kinds: Vec<KindDescriptorWire>,
}

/// One native actor's per-handler reply contract on the wire — the
/// mirror of `aether_data::name_inventory::HandlerEntry` (ADR-0109
/// §5) and the native analogue of the wasm
/// [`HandlerCapability`](aether_kinds::HandlerCapability).
/// `namespace` is the owning cap's mailbox; `id` / `name` are the
/// handler's input kind; `reply` is its reply contract (ADR-0231 §4):
/// `None` for a `-> ()` silent handler, `One(R)` for a `-> R`
/// synchronous or `-> Pending<R>` deferred reply, and `Unchecked` for an
/// unchecked handler that replies at run time with no declared kind, whose
/// stated reason rides in `reason` (#7193). Carries no
/// `doc` — the native link-time inventory holds ids + names, so a
/// native cap's per-handler docs are out of scope here (the wasm
/// `HandlerCapability` carries them from the custom section instead).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HandlerEntryWire {
    pub namespace: String,
    pub id: aether_data::KindId,
    pub name: String,
    pub reply: aether_data::ReplyContract,
    /// The unchecked handler's stated reason, present exactly for an
    /// `Unchecked` reply (#7193).
    pub reason: Option<String>,
}

/// `aether.inventory.handlers` — request the running substrate's
/// native handler manifest (ADR-0109 §5): every native chassis cap's
/// per-handler `{ namespace, input kind, reply contract }`, collected at
/// link time. Empty payload; the request *is* the signal. Mailed to
/// the `"aether.inventory"` mailbox; reply: [`HandlersResult`].
///
/// The MCP harness uses this to surface a native cap's `In -> Out`
/// the way `describe_component` surfaces a wasm component's — the
/// reply contract for the caps the driver leans on most
/// (`aether.fs`, `aether.render`, `aether.audio`).
#[aether_data::kind(name = "aether.inventory.handlers")]
pub struct ListHandlers {}

/// Reply to [`ListHandlers`] (ADR-0109 §5). One [`HandlerEntryWire`]
/// per `#[handler]` across every native actor linked into the
/// substrate, in link order. The harness folds these per `namespace`
/// so each native cap reads as a `describe_component`-style handler
/// list carrying its `In -> Out` reply contract.
///
/// No `Err` arm: the reply reads a process-global link-time table, so
/// there is no failure mode to report.
#[aether_data::kind(name = "aether.inventory.handlers_result")]
pub struct HandlersResult {
    pub handlers: Vec<HandlerEntryWire>,
}

/// `aether.inventory.memory` — request what the running engine holds, by
/// owner. Empty payload; the request *is* the signal. Mailed to the
/// `"aether.inventory"` mailbox; reply: [`ListMemoryResult`].
///
/// A debug overlay asks about once a second and draws the rows. Building
/// the reply takes the engine's memory ledger lock once and reads the
/// process's size from the platform, so it is not a per-frame question.
#[aether_data::kind(name = "aether.inventory.memory")]
pub struct ListMemory {}

/// The blob store's three byte counts. `slab_bytes` is part of
/// `resident_bytes`, and `slab_member_bytes` is the live part of
/// `slab_bytes`, so the three are never summed: `slab_bytes -
/// slab_member_bytes` is what slabs retain for entries already dropped.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobStoreBytes {
    /// Every owned entry's bytes plus every live slab, each counted once.
    pub resident_bytes: u64,
    /// Every live slab's bytes.
    pub slab_bytes: u64,
    /// Every live slab entry's bytes.
    pub slab_member_bytes: u64,
}

/// One owner's bytes under one label.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OwnerBytes {
    /// The owner's actor path (`aether.render`, `aether.kit.camera:main`),
    /// or its ADR-0064 tagged id text when the registry holds no name for
    /// it.
    pub owner: String,
    /// What the bytes are: `linear memory` for a wasm component, `textures`
    /// and `geometry` for the render capability.
    pub label: String,
    pub bytes: u64,
}

/// Reply to [`ListMemory`]: what the engine held when it was asked.
///
/// `process_bytes` is the whole process's resident set size, `None` on a
/// platform with no reader. `owners` is one row per owner and label, sorted
/// by owner then label: each live wasm component's linear memory (a
/// component and its inline children share one memory and one row), and the
/// render capability's staged textures and geometry. The rows do not sum to
/// `process_bytes`: what no owner counts (the engine's own heap, render
/// targets, pipelines, per-frame and instance buffers, audio) is in the
/// process number only, and device memory may sit outside it.
///
/// No `Err` arm: the reply reads counters, so there is no failure to report.
#[aether_data::kind(name = "aether.inventory.memory_result")]
pub struct ListMemoryResult {
    pub process_bytes: Option<u64>,
    pub blob_store: BlobStoreBytes,
    pub owners: Vec<OwnerBytes>,
}
