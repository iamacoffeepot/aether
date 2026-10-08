//! `aether.inventory` cap (ADR-0088 §6, widened by ADR-0091 §5). Serves
//! the per-build reverse-lookup inventory **and** the per-engine live
//! kind-schema registry view over mail so an out-of-process observer
//! (the MCP harness) reads the running substrate's **own, per-build**
//! state instead of a drift-prone compiled-in copy.
//!
//! Six request kinds, each replying synchronously (ADR-0112 `-> R`):
//!
//! - [`Manifest`] → [`ManifestResult`]: the compile-time manifest —
//!   every link-time [`NameEntry`](aether_data::name_inventory::NameEntry)
//!   (declared mailbox namespaces + kinds + transforms) and every
//!   [`TemplateEntry`](aether_data::name_inventory::TemplateEntry)
//!   (instanced families). Templates ship their *family shape*
//!   (`Bounded` range / `Declared` domain / `Dynamic`) so the client
//!   expands or prehashes them locally — the manifest is NOT flattened to
//!   a hash → name map (ADR-0088 §6). The client folds this once at
//!   connect and reconstructs its own static reverse map.
//! - [`Resolve`] → [`ResolveResult`]: per-id reverse lookup of
//!   dynamically-minted instance ids the client can't compute from the
//!   manifest alone (the runtime-registry arm of the ADR-0088 §2 chain):
//!   a thread id from the runtime name registry, a mailbox or kind id from
//!   the engine's live `Registry`. `None` on a miss so the client
//!   falls back to rendering the ADR-0064 tagged-id string itself.
//! - [`ResolveAddress`] → [`ResolveAddressResult`]: engine-owned
//!   canonical/ADR-0166 short address resolution to one live actor's
//!   canonical path.
//! - [`ListKinds`] → [`ListKindsResult`] (ADR-0091): every
//!   [`KindId`](aether_data::KindId) currently registered in the
//!   substrate's `Registry`, with its full
//!   [`SchemaType`](aether_data::SchemaType). The harness folds the
//!   reply into a per-engine encode cache so a `send_mail` against a
//!   component-defined kind encodes correctly the moment the
//!   `aether.component.load` returns — no per-kind hand-promotion into
//!   `aether-kinds`.
//! - [`ListHandlers`] → [`HandlersResult`] (ADR-0109 §5): the native
//!   handler manifest — every `#[handler]`'s `{ namespace, input kind,
//!   reply contract }` across every native actor linked into the substrate,
//!   read from the link-time
//!   [`HandlerEntry`](aether_data::name_inventory::HandlerEntry)
//!   inventory the `#[actor]` macro populates. The native analogue of
//!   the wasm `aether.kinds.inputs` custom section: the harness folds
//!   the reply per `namespace` so a native cap (`aether.fs`,
//!   `aether.render`, …) surfaces its `In -> Out` the way
//!   `describe_component` surfaces a wasm component's.
//! - [`ListMemory`] → [`ListMemoryResult`]: what the engine holds, by
//!   owner — the process's resident set size, the blob store's three byte
//!   counts, and one row per owner and label from the engine's memory
//!   ledger (each live wasm component's linear memory, the render
//!   capability's staged textures and geometry). A debug overlay asks about
//!   once a second.
//!
//! The `Manifest` / `Resolve` / `ResolveAddress` / `ListKinds` /
//! `ListHandlers` / `ListMemory` family is
//! owned here in [`kinds`], per the `capability-anatomy.md` rule. Its one
//! upstream consumer, `aether-mcp`, takes this crate identity-only
//! (`default-features = false`) exactly as it takes `aether-fs` — the
//! kinds' `Kind`-derived inventory submissions land in the harness's
//! static `descriptors::all()` vocabulary through that link.
//! [`KindDescriptorWire`](aether_kinds::KindDescriptorWire) is the one
//! holdout in `aether-kinds`: `aether-fleet` uses it for component config
//! descriptors, so it is shared vocabulary rather than cap-owned.
//!
//! The cap is stateless. `ListMemory` reads the engine's memory ledger
//! through `NativeCtx::memory_report`. `ResolveAddress`, `ListKinds`, and `Resolve`'s
//! mailbox and kind ids read the engine's `Registry` through handler ctx read
//! verbs (`NativeCtx::canonical_path`, `kind_descriptors`, `tagged_id_name`)
//! — the same registry the component-host cap stages its loaded kinds into,
//! so a `load_component`'s registrations are visible the moment the owner
//! publishes them; no event channel, no cache invalidation, and no handle
//! pinning one registry instance for the cap's lifetime. The manifest and
//! handlers arms, and `Resolve`'s thread ids, read process-global tables.
//! `#[actor(singleton)]` auto-submits its own `NameEntry` for
//! `NAMESPACE`, so `aether.inventory` reverses through the same static
//! map it serves.

#![forbid(unsafe_code)]

// `#[actor]` names both the request kinds (the `HandlesKind` markers) and the
// single-reply kinds (the reply markers and `Contract` rows) on every target,
// outside the `feature = "runtime"` gate, so a guest build, a transport-only
// native build, and a runtime build all need every one in scope at module root.
use kinds::{
    HandlersResult, ListHandlers, ListKinds, ListKindsResult, ListMemory, ListMemoryResult, Manifest, ManifestResult,
    Resolve, ResolveAddress, ResolveAddressResult, ResolveResult,
};

use aether_actor::actor;

pub mod kinds;

/// `aether.inventory` cap **identity** (ADR-0122 identity/runtime split,
/// ADR-0088 §6, widened by ADR-0091 §5). A ZST carrying only the
/// addressing — the `Addressable` / `HandlesKind` markers and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// `#[runtime] impl` lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never pulls `aether_substrate` through this cap.
///
/// The cap carries no runtime state — `InventoryCapabilityState` is a
/// ZST, named only because a struct-hosted split identity cannot use
/// `type State = Self` (that spelling selects the un-split shape). The
/// `Manifest` and `ListHandlers` arms read the process-global link-time
/// inventories directly. The `ResolveAddress`, `ListKinds`, and `Resolve`
/// arms read the engine's `Registry` through handler ctx read verbs
/// (`Resolve` names a thread id from the runtime name registry), so their
/// replies reflect its live address and vocabulary state (including anything
/// `ComponentHostCapability` registered at load time) without a cross-cap
/// event channel or pinning one registry instance at `init`.
#[actor(singleton, root)]
pub struct InventoryCapability;

// The runtime half — the `aether_substrate`-typed imports, the state
// struct, the `#[runtime] impl`, and the cap's tests — lives under the
// `runtime` directory, gated once here. Nothing in this file names a runtime type directly,
// so there is no `use runtime::*` glob (matching `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;
