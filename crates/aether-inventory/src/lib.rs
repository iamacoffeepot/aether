//! `aether.inventory` cap (ADR-0088 §6, widened by ADR-0091 §5). Serves
//! the per-build reverse-lookup inventory **and** the per-engine live
//! kind-schema registry view over mail so an out-of-process observer
//! (the MCP harness) reads the running substrate's **own, per-build**
//! state instead of a drift-prone compiled-in copy.
//!
//! Five request kinds, each replying synchronously (ADR-0112 `-> R`):
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
//!   manifest alone (the runtime-registry arm of the ADR-0088 §2 chain,
//!   `thread_name::resolve_runtime`). `None` on a miss so the client
//!   falls back to rendering the ADR-0064 tagged-id string itself.
//! - [`ResolveAddress`] → [`ResolveAddressResult`]: engine-owned
//!   canonical/ADR-0166 abbreviated address resolution to one live mailbox id
//!   and canonical path.
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
//!   reply kind }` across every native actor linked into the substrate,
//!   read from the link-time
//!   [`HandlerEntry`](aether_data::name_inventory::HandlerEntry)
//!   inventory the `#[actor]` macro populates. The native analogue of
//!   the wasm `aether.kinds.inputs` custom section: the harness folds
//!   the reply per `namespace` so a native cap (`aether.fs`,
//!   `aether.render`, …) surfaces its `In -> Out` the way
//!   `describe_component` surfaces a wasm component's.
//!
//! The `Manifest` / `Resolve` / `ResolveAddress` / `ListKinds` /
//! `ListHandlers` family is
//! owned here in [`kinds`], per the `capability-anatomy.md` rule. Its one
//! upstream consumer, `aether-mcp`, takes this crate identity-only
//! (`default-features = false`) exactly as it takes `aether-fs` — the
//! kinds' `Kind`-derived inventory submissions land in the harness's
//! static `descriptors::all()` vocabulary through that link.
//! [`KindDescriptorWire`](aether_kinds::KindDescriptorWire) is the one
//! holdout in `aether-kinds`: `aether-fleet` uses it for component config
//! descriptors, so it is shared vocabulary rather than cap-owned.
//!
//! The cap is stateless. `ResolveAddress` and `ListKinds` read the engine's `Registry`
//! through the handler ctx (`NativeCtx::mailer().registry()`) — the same
//! one the component-host cap registers into for `register_or_match_all`,
//! so a `load_component`'s registrations are visible the moment they
//! return; no event channel, no cache invalidation, and no `Arc` clone
//! pinning one registry instance for the cap's lifetime. The manifest /
//! resolve / handlers arms read process-global link-time tables.
//! `#[actor(singleton)]` auto-submits its own `NameEntry` for
//! `NAMESPACE`, so `aether.inventory` reverses through the same static
//! map it serves.

// Handler-signature kinds must be importable at module root because
// `#[actor]` emits `impl HandlesKind<K> for InventoryCapability {}`
// markers always-on, outside the `feature = "runtime"` gate. The reply
// kinds are named only by the gated handler bodies, so they ride the
// runtime gate below.
use kinds::{ListHandlers, ListKinds, Manifest, Resolve, ResolveAddress};

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
/// `Manifest`, `Resolve`, and `ListHandlers` arms read process-global
/// tables — the link-time inventories and the runtime name registry —
/// directly. The `ResolveAddress` and `ListKinds` arms read the engine's
/// `Registry` off the handler ctx, so their replies reflect its live address
/// and vocabulary state (including anything `ComponentHostCapability`
/// registered at load time) without a cross-cap event channel or pinning one
/// registry instance at `init`.
#[actor(singleton)]
pub struct InventoryCapability;

// The reply kinds ride the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names each handler's reply kind `::ID`,
// so a transport-only build must still see them. The wire-projection
// helpers, the `aether_substrate`-typed imports, and the state struct
// sit behind the one `feature = "runtime"` gate.
#[cfg(not(target_family = "wasm"))]
use kinds::{HandlersResult, ListKindsResult, ManifestResult, ResolveAddressResult, ResolveResult};

// The runtime half — the wire-projection helpers (nested as
// `runtime/manifest.rs` / `runtime/resolve.rs`), the
// `aether_substrate`-typed imports, the state struct, the `#[runtime]
// impl`, and the cap's tests — lives under the `runtime` directory,
// gated once here. Nothing in this file names a runtime type directly,
// so there is no `use runtime::*` glob (matching `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;
