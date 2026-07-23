//! The `aether.inventory` request / reply vocabulary — the cap's
//! caller-facing kinds (ADR-0088 §6, ADR-0091, ADR-0109 §5), owned here
//! per the `capability-anatomy.md` kind-ownership rule.
//!
//! The `aether.inventory` mailbox serves the per-build reverse-lookup
//! inventory over mail so an out-of-process observer (the MCP harness)
//! reads the running substrate's *own* state instead of a drift-prone
//! compiled-in copy. Four request kinds:
//!
//! - [`Manifest`] → the compile-time manifest: every declared
//!   `NameEntry` + every instanced-family `TemplateEntry`. Templates keep
//!   their *family shape* (the client expands a `Bounded` range /
//!   `Declared` domain itself); the manifest does NOT flatten to a
//!   hash → name map (ADR-0088 §6).
//! - [`Resolve`] → per-id `Option<String>`, for ids the client can't
//!   compute from the manifest alone (ADR-0088 §5).
//! - [`ListKinds`] → the engine's live kind vocabulary (ADR-0091).
//! - [`ListHandlers`] → the native handler manifest (ADR-0109 §5).
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.param_kind")]
pub enum ParamKindWire {
    /// Finite inclusive integer range (`aether-worker-{0..=255}`).
    /// The client enumerates `lo..=hi`, substitutes each value into
    /// the template, and hashes the result for an exact reverse.
    Bounded { lo: u64, hi: u64 },
    /// The hole ranges over every [`NameEntryWire`] whose `domain`
    /// equals `domain` (`aether-root-{NAMESPACE}` over the declared
    /// mailbox namespaces).
    Declared { domain: Vec<u8> },
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
    pub domain: Vec<u8>,
    pub template: String,
    pub param: ParamKindWire,
}

/// `aether.inventory.manifest` — request the running substrate's
/// compile-time reverse-lookup manifest (ADR-0088 §6). Empty payload;
/// the request *is* the signal. Mailed to the `"aether.inventory"`
/// mailbox; reply: [`ManifestResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.manifest")]
pub struct Manifest {}

/// Reply to [`Manifest`] (ADR-0088 §6). Carries every link-time
/// [`NameEntryWire`] (declared names: chassis mailbox namespaces +
/// kinds + transforms) and every [`TemplateEntryWire`] (instanced
/// families, `Bounded`/`Declared`/`Dynamic`). The client folds
/// `names` into a hash → name map and expands `Bounded`/`Declared`
/// templates locally; `Dynamic` templates resolve per-id via
/// [`Resolve`]. This is the *authoritative, per-build* inventory —
/// the served form is always the running substrate's own.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.manifest_result")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.resolve")]
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.resolve_result")]
pub struct ResolveResult {
    pub resolved: Vec<ResolvedName>,
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
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.kinds")]
pub struct ListKinds {}

/// Reply to [`ListKinds`] (ADR-0091). One [`KindDescriptorWire`] per
/// kind currently registered in the substrate's `Registry`, sorted
/// by name (the registry's `list_kind_descriptors` ordering). The
/// harness folds this into its per-engine encode cache; component-
/// defined kinds (loaded via `aether.component.load`) show up here
/// alongside the substrate's static vocabulary the moment the load
/// returns, no separate notification.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.kinds_result")]
pub struct ListKindsResult {
    pub kinds: Vec<KindDescriptorWire>,
}

/// One native actor's per-handler reply contract on the wire — the
/// mirror of `aether_data::name_inventory::HandlerEntry` (ADR-0109
/// §5) and the native analogue of the wasm
/// [`HandlerCapability`](aether_kinds::HandlerCapability).
/// `namespace` is the owning cap's mailbox; `id` / `name` are the
/// handler's input kind; `reply` is its declared reply kind id
/// (`None` for a `-> ()` fire-and-forget handler, `Some` for a
/// `-> R` synchronous or `-> Pending<R>` deferred reply). Carries no
/// `doc` — the native link-time inventory holds ids + names, so a
/// native cap's per-handler docs are out of scope here (the wasm
/// `HandlerCapability` carries them from the custom section instead).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HandlerEntryWire {
    pub namespace: String,
    pub id: aether_data::KindId,
    pub name: String,
    pub reply: Option<aether_data::KindId>,
}

/// `aether.inventory.handlers` — request the running substrate's
/// native handler manifest (ADR-0109 §5): every native chassis cap's
/// per-handler `{ namespace, input kind, reply kind }`, collected at
/// link time. Empty payload; the request *is* the signal. Mailed to
/// the `"aether.inventory"` mailbox; reply: [`HandlersResult`].
///
/// The MCP harness uses this to surface a native cap's `In -> Out`
/// the way `describe_component` surfaces a wasm component's — the
/// reply contract for the caps the driver leans on most
/// (`aether.fs`, `aether.render`, `aether.audio`).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.handlers")]
pub struct ListHandlers {}

/// Reply to [`ListHandlers`] (ADR-0109 §5). One [`HandlerEntryWire`]
/// per `#[handler]` across every native actor linked into the
/// substrate, in link order. The harness folds these per `namespace`
/// so each native cap reads as a `describe_component`-style handler
/// list carrying its `In -> Out` reply contract.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.inventory.handlers_result")]
pub struct HandlersResult {
    pub handlers: Vec<HandlerEntryWire>,
}
