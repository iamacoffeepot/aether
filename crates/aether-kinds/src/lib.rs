//! aether-kinds: the substrate's own mail vocabulary. Imported by any
//! actor that wants to send mail to the substrate, receive mail the
//! substrate dispatches (tick, input), or consume the substrate's sink
//! kinds (`draw_triangle`). See ADR-0005 / ADR-0030.
//!
//! Kind ids are `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` — a compile-time
//! const on the `Kind` trait (ADR-0030 Phase 2). Substrate boot and
//! guest SDK arrive at the same id independently; no host-fn resolve
//! round-trip. Consumers address kinds via the `NAME` constants and
//! the derived `ID` constants on the impls below.

#![no_std]

extern crate alloc;

pub mod descriptors;
pub mod diagnostics;
pub mod input;
pub mod keycode;
pub mod lifecycle;
pub mod math;
pub mod mouse_button;
pub mod text_metrics;
pub mod trace;
pub mod utility;
// First-party native `#[transform]`s (ADR-0048, issue 1464). Native-only
// (no wasm consumer runs transforms) and co-located with the `Mat4Apply`
// kind the sole transform consumes. Holds only the generic `mat4_apply`.
#[cfg(not(target_family = "wasm"))]
pub mod transforms;

pub use text_metrics::{CachedFontMetrics, scale_units};

pub use diagnostics::{MonitorNotice, UnresolvedMail};
pub use input::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};
pub use lifecycle::{
    InitCaps, InitComponents, LifecycleAdvance, LifecycleAdvanceComplete, LifecycleSubscribe, LifecycleSubscribeResult,
    LifecycleSubscribeSelf, LifecycleUnsubscribe, LifecycleUnsubscribeAll, LifecycleUnsubscribeSelf, Present, Quit,
    Render, Shutdown, Tick,
};
pub use math::Mat4Apply;
pub use utility::{Ping, Pong};

// The render cap's drawing/texture kinds — `Vertex` / `DrawTriangle` /
// `DRAW_TRIANGLE_BYTES` / `ViewProjection` and the `aether.render.*` texture +
// quad family — moved to `aether_render::kinds` (ADR-0121,
// "capabilities own their kinds"). The capture-request and `FrameCheck`
// verification kinds stay below: `aether-mcp` and the substrate core
// consume them, so moving them would close a dependency cycle.

// `aether.kit.camera.*` control kinds (CameraCreate / CameraDestroy /
// CameraSetActive / CameraSetMode / CameraOrbitSet / CameraTopdownSet)
// live in `mod control_plane` below — they're structured because
// every one carries a `String` name and `Option<...>` per-field
// deltas, so they can't ride the cast-shaped path.
// Reserved control-plane vocabulary (ADR-0010). The substrate handles
// these kinds inline rather than dispatching to a component — the
// namespace itself is the routing discriminator. ADR-0019 PR 5 turned
// these from Opaque markers into real schema-described types: their
// fields are structured-encoded on the wire, hub-encodable from agent
// params (no more `payload_bytes` workaround), and the substrate
// decodes them with `wire::from_bytes` against the same types
// that ship as the kind.
//
// Gated behind `descriptors` because the types use `String`/`Vec`/
// `Option` — wasm guests that don't enable descriptors stay free of
// the alloc-heavy payload types (and have no business loading
// components anyway).

pub use control_plane::*;
pub use engine::*;

mod engine {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::KindDescriptorWire;
    use serde::{Deserialize, Serialize};

    /// `aether.engine.list` — ask the engines cap (`aether.engine`) to
    /// enumerate every engine it currently supervises. Fieldless
    /// request; the reply is a [`ListEnginesResult`]. Issue 763 P4.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    #[kind(name = "aether.engine.list")]
    pub struct ListEngines {}

    /// One supervised engine, as reported in a [`ListEnginesResult`].
    ///
    /// `engine_id` is the plain UUID string the engines cap minted at
    /// spawn time — `EngineId` itself doesn't implement `Schema`, so
    /// the wire carries the string form (the same convention the
    /// `aether.process.*` kinds use). `rpc_port` is the localhost port
    /// the cap assigned the substrate's `RpcServerCapability`.
    ///
    /// `last_heartbeat_age_millis` is how long ago the cap last saw a
    /// liveness signal from this engine (issue 1339) — `0` right after
    /// spawn, refreshed each time the engine's proxy confirms a `Pong`.
    /// A value climbing past the heartbeat interval means the engine is
    /// going stale; the cap evicts it (drops it from this list) once it
    /// crosses the miss limit.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct EngineDescriptor {
        pub engine_id: String,
        pub rpc_port: u16,
        pub last_heartbeat_age_millis: u64,
    }

    /// Why an engine left the supervised-engine table, as carried in a
    /// [`DeadEngineDescriptor`] (and in the `EngineDied` signal for the
    /// two self-death paths). A tagged enum so an observer can branch on
    /// the cause without parsing free text; the `detail` string on the
    /// non-clean variants carries the specifics.
    ///
    /// - `Terminated` — a deliberate `aether.engine.terminate` shut the
    ///   engine down. The clean-shutdown case; carries no detail.
    /// - `Crashed { detail }` — the substrate closed its RPC connection
    ///   (`Bye` / eof) on its own; `detail` is the close reason the proxy
    ///   observed.
    /// - `Evicted { detail }` — the liveness heartbeat crossed its miss
    ///   limit and the proxy declared the engine dead; `detail` is the
    ///   `heartbeat miss limit N of M` count.
    /// - `SpawnFailed { detail }` — the spawn never connected: the
    ///   substrate failed to come up (fork / materialize error, or the
    ///   proxy connect/boot failed), so it was never registered alive.
    ///   `detail` is the proxy connect / boot error. Distinct from
    ///   `Crashed`, which is a registered substrate that later closed its
    ///   connection.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum DeathReason {
        Terminated,
        Crashed { detail: String },
        Evicted { detail: String },
        SpawnFailed { detail: String },
    }

    /// One recently-departed engine, as reported in a
    /// [`ListEnginesResult`]'s `recently_died` ring (the bounded last-N
    /// deaths the engines cap retains). Distinct from [`EngineDescriptor`]:
    /// a dead engine carries a [`DeathReason`] and an age-since-death
    /// rather than a live heartbeat age. `engine_id` / `rpc_port` are the
    /// same identifiers it carried while alive; `died_age_millis` is how
    /// long ago the cap removed it from the supervised table.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct DeadEngineDescriptor {
        pub engine_id: String,
        pub rpc_port: u16,
        pub reason: DeathReason,
        pub died_age_millis: u64,
    }

    /// `aether.engine.list_result` — reply to [`ListEngines`]: every
    /// engine the cap supervises right now, plus a bounded sidecar of the
    /// engines that recently left and why. Issue 763 P4.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_result")]
    pub struct ListEnginesResult {
        pub engines: Vec<EngineDescriptor>,
        /// The recently-died ring: the last few engines that left the
        /// supervised table, each with why it left ([`DeathReason`]) and
        /// how long ago. Lets an observer tell a clean `terminate` from a
        /// crash or a heartbeat eviction without grepping host logs.
        pub recently_died: Vec<DeadEngineDescriptor>,
    }

    /// How a [`SpawnEngine`] names the binary to fork — a registry
    /// selector resolved against the hub's content-addressed binary store
    /// (ADR-0115), not a host filesystem path. The engines cap resolves it
    /// in this order:
    ///
    /// - `query` is the exact selector token: a sha256 content `hash`, a
    ///   `name@version` (where `version` is the binary's self-reported
    ///   build id — its manifest `git_sha`), or a `name` an upload pointed
    ///   at a hash. `None` means `default` — the configured fallback, the
    ///   `headless` chassis (no window, runs on any host), so a bare
    ///   `SpawnEngine` with an empty selector returns a working engine.
    /// - `chassis` / `caps` / `target` are an attribute query over the
    ///   stored manifests, consulted when `query` is `None`: keep only
    ///   binaries whose `chassis` matches, whose linked `caps` are a
    ///   superset of every listed cap, and whose `target` triple matches.
    ///   They mirror [`ListEngineBinaries`]' filter shape.
    ///
    /// An exact `query` wins first; absent one, the attribute query
    /// resolves, then `default`. A selector that resolves to no stored
    /// binary fails the spawn.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct BinarySelector {
        pub query: Option<String>,
        pub chassis: Option<String>,
        pub caps: Vec<String>,
        pub target: Option<String>,
    }

    /// `aether.engine.spawn` — ask the engines cap to fork+exec a
    /// substrate binary and connect a per-engine proxy to it. Issue
    /// 763 P4.
    ///
    /// The cap resolves `selector` against its content-addressed binary
    /// store (ADR-0115) to the stored content bytes, materializes them to
    /// an executable temp file, picks a free localhost port for the
    /// substrate's `RpcServerCapability`, injects it as `AETHER_RPC_PORT`,
    /// forks the realized binary with `args` forwarded verbatim, then
    /// boots an `aether.engine.proxy:<id>` actor that dials it. Reply:
    /// [`SpawnEngineResult`] — `Err` if the selector resolves to no stored
    /// binary. The host filesystem path is gone from the spawn surface;
    /// the only path input is the one-time [`UploadBinary`].
    ///
    /// `boot_manifest` (when `Some`) is the absolute path to a
    /// `BundleManifest` JSON of components to auto-load at boot; the cap
    /// injects it as `AETHER_BOOT_MANIFEST` alongside `AETHER_RPC_PORT`,
    /// and the spawned chassis reads the listed wasm itself (spawn is
    /// single-host) so the engine comes up with those components already
    /// loading — no follow-up `load_component` round-trips. `None` boots
    /// a bare engine, the pre-existing behaviour.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.spawn")]
    pub struct SpawnEngine {
        pub selector: BinarySelector,
        pub args: Vec<String>,
        pub boot_manifest: Option<String>,
    }

    /// Reply to [`SpawnEngine`]. Issue 763 P4.
    ///
    /// `Ok` carries the freshly minted `engine_id` (plain UUID string —
    /// pass it back to [`TerminateEngine`]) and the `rpc_port` the cap
    /// assigned. `Err` carries a free-form reason — fork failure, or
    /// the proxy failing to connect within the substrate's startup
    /// window — plus `engine_id`, the allocated id when the failure came
    /// after the cap minted one (`None` for a pre-allocation failure
    /// like a selector miss or a port-allocation error). A failed spawn
    /// with `engine_id = Some(_)` also leaves a matching `SpawnFailed`
    /// entry in [`ListEnginesResult`]'s `recently_died` ring, so a caller
    /// can correlate and reap. On `Err` no child process is left running.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.spawn_result")]
    pub enum SpawnEngineResult {
        Ok { engine_id: String, rpc_port: u16 },
        Err { engine_id: Option<String>, error: String },
    }

    /// `aether.engine.terminate` — ask the engines cap to shut down a
    /// supervised engine. Issue 763 P4.
    ///
    /// The cap forwards this kind to the engine's
    /// `aether.engine.proxy:<id>` actor, which SIGKILLs the child
    /// substrate it forked and self-shuts-down. `engine_id` is the
    /// plain UUID string from [`SpawnEngineResult`] /
    /// [`ListEnginesResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.terminate")]
    pub struct TerminateEngine {
        pub engine_id: String,
    }

    /// Reply to [`TerminateEngine`]. Issue 763 P4. `Err` is for an
    /// `engine_id` that doesn't parse or names no supervised engine.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.terminate_result")]
    pub enum TerminateEngineResult {
        Ok,
        Err { error: String },
    }

    /// What a stored binary *is*, captured once at upload time by forking
    /// the binary with `--describe` (ADR-0115, issue 1953). The
    /// content-addressed store sidecars one of these next to each entry's
    /// bytes, and [`ListEngineBinariesResult`] returns it per entry so an
    /// observer (and the spawn cutover, #1954) can tell a `headless` from
    /// a `desktop` binary, see which capabilities it links, and read its
    /// build provenance — all without re-running the binary.
    ///
    /// - `chassis` — the chassis profile (`Chassis::PROFILE`):
    ///   `"headless"` / `"desktop"` / `"hub"`.
    /// - `caps` — the mailbox namespaces the chassis registers (its
    ///   linked capabilities, e.g. `aether.fs`, `aether.render`).
    /// - `git_sha` / `profile` / `target` — build provenance from the
    ///   bundle's `build.rs` (`git rev-parse --short HEAD`, the cargo
    ///   build profile, the target triple); `git_sha` is `"unknown"` when
    ///   the binary was built outside a git checkout.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct BinaryManifest {
        pub chassis: String,
        pub caps: Vec<String>,
        pub git_sha: String,
        pub profile: String,
        pub target: String,
    }

    /// One stored binary in a [`ListEngineBinariesResult`] (ADR-0115, issue
    /// 1953). `hash` is the sha256 hex over the binary's raw bytes — the
    /// content-address key. `name` is the optional human-readable name an
    /// upload pointed at this hash (the latest upload that named it wins).
    /// `manifest` is what the binary reported via `--describe`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct BinaryEntry {
        pub hash: String,
        pub name: Option<String>,
        pub manifest: BinaryManifest,
    }

    /// `aether.engine.upload_binary` — ingest a binary into the hub's
    /// content-addressed store (ADR-0115, issue 1953). `staged_path` is an
    /// absolute host path the hub reads itself (aether-mcp never reads the
    /// bytes — a binary is too large to ride the tool channel); the cap
    /// sha256-hashes the bytes, dedups against the existing store, forks
    /// `staged_path --describe` to capture its [`BinaryManifest`], and
    /// stores both. `name`, when set, points that human-readable name at
    /// the resulting hash. Reply: [`UploadBinaryResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_binary")]
    pub struct UploadBinary {
        pub staged_path: String,
        pub name: Option<String>,
    }

    /// Reply to [`UploadBinary`] (ADR-0115, issue 1953). `Ok` carries the
    /// content-address `hash` the bytes stored under (a re-upload of
    /// identical bytes returns the same hash) and the `name` now pointing
    /// at it, if any. `Err` carries a free-form reason — an unreadable
    /// `staged_path`, or a `--describe` that failed or didn't yield a
    /// parseable manifest.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_binary_result")]
    pub enum UploadBinaryResult {
        Ok { hash: String, name: Option<String> },
        Err { error: String },
    }

    /// `aether.engine.list_binaries` — enumerate the hub's stored binaries
    /// (ADR-0115, issue 1953). The filter fields are AND-combined and each
    /// defaults to "no constraint": `chassis` keeps only entries whose
    /// `manifest.chassis` matches, `caps` keeps only entries whose
    /// `manifest.caps` is a superset of every listed cap, `target` keeps
    /// only entries whose `manifest.target` matches. `include_history`
    /// controls whether unnamed historical hashes are included (the default
    /// is the live, name-pointed registry only), and `limit` caps the page
    /// after filtering (`None` defaults to 20; `Some(0)` returns no entries).
    /// Reply: [`ListEngineBinariesResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    #[kind(name = "aether.engine.list_binaries")]
    pub struct ListEngineBinaries {
        pub chassis: Option<String>,
        pub caps: Vec<String>,
        pub target: Option<String>,
        pub limit: Option<u32>,
        pub include_history: bool,
    }

    /// Reply to [`ListEngineBinaries`] (ADR-0115, issues 1953 and 3007): a
    /// newest-first page of stored binaries matching the request, each as a
    /// [`BinaryEntry`] carrying its hash, optional name, and `--describe`
    /// manifest. `total_matched` is counted after attribute/history filtering
    /// and before the requested limit is applied.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_binaries_result")]
    pub struct ListEngineBinariesResult {
        pub binaries: Vec<BinaryEntry>,
        pub total_matched: u32,
    }

    /// What a stored component *is*, read straight from the wasm at upload
    /// time — no `--describe` execution step (ADR-0116, issue 1956). A
    /// component embeds its manifest in the `aether.kinds.inputs` /
    /// `aether.namespace` custom sections (ADR-0028 / ADR-0033 / ADR-0096),
    /// so the hub reads it with `wasmparser`, the same reader the substrate
    /// uses at load. The store sidecars one of these next to each
    /// component entry's bytes, and [`ListComponentBinariesResult`] returns
    /// it per entry so an observer (and the resolve query) can select a component
    /// by what it is.
    ///
    /// - `namespaces` — every exported actor's `Addressable::NAMESPACE`. A
    ///   single-actor module yields one; a multi-actor module
    ///   (`export!(A, B, …)`) yields one per type, the default type first.
    /// - `actors` — one [`ComponentActor`] per exported actor type, the
    ///   `module@actor` selector axis (ADR-0096 export selector).
    /// - `handled_kinds` — the union of every actor's handled `KindId`s
    ///   (ADR-0030), the handled-kind selector axis.
    /// - `fallback` — whether any exported actor declares a `#[fallback]`.
    /// - `provenance` — the wasm `producers` custom section rendered as a
    ///   short string (`"<tool> <version>; …"`), or empty when absent.
    /// - `default_entry` — the bare-load default actor's namespace per
    ///   ADR-0138; `None` for a defaultless multi-actor module (built with
    ///   a bare `export!(A, B, …)`), `Some(ns)` for a single-actor module
    ///   or a multi-actor module that opted in via `export!(default = A, …)`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentManifest {
        pub namespaces: Vec<String>,
        pub actors: Vec<ComponentActor>,
        pub handled_kinds: Vec<aether_data::KindId>,
        pub fallback: bool,
        pub provenance: String,
        #[serde(default)]
        pub default_entry: Option<String>,
    }

    /// One exported actor type within a (possibly multi-actor) component
    /// module (ADR-0096, issue 1956). `namespace` is the type's
    /// `Addressable::NAMESPACE` — the `@actor` half of a `module@actor` selector;
    /// `handled_kinds` is the kind ids this actor handles; `fallback` is
    /// whether it declares a `#[fallback]`. A single-actor module's
    /// implicit group reports `namespace` as the module's `aether.namespace`
    /// section value.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentActor {
        pub namespace: String,
        pub handled_kinds: Vec<aether_data::KindId>,
        pub fallback: bool,
    }

    /// One stored component in a [`ListComponentBinariesResult`] (ADR-0116,
    /// issue 1956). `hash` is the sha256 hex over the wasm's raw bytes — the
    /// content-address key. `name` is the optional human-readable name the
    /// latest upload pointed at this hash (`Addressable::NAMESPACE` is the
    /// natural one). `manifest` is what the wasm self-reported.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct ComponentEntry {
        pub hash: String,
        pub name: Option<String>,
        pub manifest: ComponentManifest,
    }

    /// How a [`ResolveComponent`] names the component wasm to load — a
    /// registry selector resolved against the hub's content-addressed store
    /// (ADR-0116), never a host filesystem path (the path is retired from
    /// `load_component` entirely). The engines cap resolves it in this
    /// order:
    ///
    /// - `query` is the exact selector token: a sha256 content `hash`, a
    ///   `name@version` (treated as `name` latest in v1 — no per-name
    ///   version index yet, ADR-0116), or a `name` an upload pointed at a
    ///   hash. `module@actor` resolves the `module` part as the
    ///   `name`/`hash` and the `@actor` part as the [`ResolveComponentResult`]
    ///   `export` (the actor `Addressable::NAMESPACE` to instantiate, ADR-0096).
    /// - `namespace` / `handled_kind` are an attribute query over the
    ///   type-tagged component manifests, consulted when `query` is `None`:
    ///   keep only components exporting that `namespace`, or handling that
    ///   `KindId`. An attribute query that matches more than one component
    ///   is a clean ambiguity error, not a silent pick.
    ///
    /// An exact `query` wins first; absent one, the attribute query
    /// resolves. A selector that resolves to no stored component fails.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ComponentSelector {
        pub query: Option<String>,
        pub namespace: Option<String>,
        pub handled_kind: Option<aether_data::KindId>,
    }

    /// `aether.engine.upload_component` — ingest a component wasm into the
    /// hub's content-addressed store (ADR-0116, issue 1956). `staged_path`
    /// is an absolute host path the hub reads itself (aether-mcp never reads
    /// the bytes — too large for the tool channel); the cap sha256-hashes
    /// the bytes, dedups against the existing store, reads the manifest
    /// straight from the wasm (no execution step — `aether.kinds.inputs` +
    /// `aether.namespace` + the `producers` section), and stores both.
    /// `name`, when set, points that human-readable name at the resulting
    /// hash. Reply: [`UploadComponentResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_component")]
    pub struct UploadComponent {
        pub staged_path: String,
        pub name: Option<String>,
    }

    /// Reply to [`UploadComponent`] (ADR-0116, issue 1956). `Ok` carries the
    /// content-address `hash` the bytes stored under (a re-upload of
    /// identical bytes returns the same hash) and the `name` now pointing
    /// at it, if any. `Err` carries a free-form reason — an unreadable
    /// `staged_path` or a wasm whose manifest can't be parsed.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.upload_component_result")]
    pub enum UploadComponentResult {
        Ok { hash: String, name: Option<String> },
        Err { error: String },
    }

    /// `aether.engine.resolve_component` — resolve a [`ComponentSelector`]
    /// to a stored component's wasm bytes + manifest (ADR-0116, issue
    /// 1956). aether-mcp calls this hub-local before forwarding a
    /// `LoadComponent` to the target substrate, so the resolve hop keeps the
    /// load seam path-free. Reply: [`ResolveComponentResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.resolve_component")]
    pub struct ResolveComponent {
        pub selector: ComponentSelector,
    }

    /// Reply to [`ResolveComponent`] (ADR-0116, issue 1956). `Ok` carries
    /// the resolved content `hash`, the `wasm` bytes the load forwards, the
    /// `name` pointing at the hash (if any), the `manifest` the store read
    /// from the wasm, and `export` — the `@actor` half of a `module@actor`
    /// selector, threaded into the forwarded `LoadComponent.export` so a
    /// specific actor type is instantiated from a multi-actor module
    /// (ADR-0096); `None` for a plain selector. `Err` carries a free-form
    /// reason — a selector that resolves to no stored component, or an
    /// attribute query matching more than one (a clean ambiguity error).
    #[allow(clippy::large_enum_variant)]
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.resolve_component_result")]
    pub enum ResolveComponentResult {
        Ok {
            hash: String,
            wasm: Vec<u8>,
            name: Option<String>,
            manifest: ComponentManifest,
            export: Option<String>,
            /// Full schema descriptor for the component's typed
            /// init-config kind, when it declares one. Added after the
            /// original ADR-0116 resolve reply, so older stored/forwarded
            /// values decode with `None`.
            #[serde(default)]
            config_kind: Option<KindDescriptorWire>,
        },
        Err {
            error: String,
        },
    }

    /// `aether.engine.list_components` — enumerate the hub's stored
    /// components (ADR-0116, issue 1956). The filter fields are
    /// AND-combined and each defaults to "no constraint": `namespace` keeps
    /// only entries exporting that actor namespace, `handled_kind` keeps
    /// only entries handling that `KindId`. `include_history` controls whether
    /// unnamed historical hashes are included (the default is the live,
    /// name-pointed registry only), and `limit` caps the page after filtering
    /// (`None` defaults to 20; `Some(0)` returns no entries). Reply:
    /// [`ListComponentBinariesResult`].
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    #[kind(name = "aether.engine.list_components")]
    pub struct ListComponentBinaries {
        pub namespace: Option<String>,
        pub handled_kind: Option<aether_data::KindId>,
        pub limit: Option<u32>,
        pub include_history: bool,
    }

    /// Reply to [`ListComponentBinaries`] (ADR-0116, issues 1956 and 3007): a
    /// newest-first page of stored components matching the request, each as a
    /// [`ComponentEntry`] carrying its hash, optional name, and the manifest
    /// read from the wasm. `total_matched` is counted after attribute/history
    /// filtering and before the requested limit is applied.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.engine.list_components_result")]
    pub struct ListComponentBinariesResult {
        pub components: Vec<ComponentEntry>,
        pub total_matched: u32,
    }
}

mod control_plane {
    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;

    use serde::{Deserialize, Serialize};

    /// `aether.component.load` — request the substrate load a WASM
    /// component into a freshly allocated mailbox. Carries the raw
    /// WASM bytes and an optional human-readable name. The
    /// component's kind vocabulary ships embedded in the wasm's
    /// `aether.kinds` custom section (ADR-0028) — the substrate
    /// reads it directly and the loader doesn't need to declare
    /// anything. Substrate replies with `LoadResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.load")]
    pub struct LoadComponent {
        pub wasm: Vec<u8>,
        pub name: Option<String>,
        /// ADR-0090 (issue 1257): optional init-config bytes handed to
        /// the guest's typed `WasmActor::init` at instantiate-time. An
        /// empty vec means "no config" — the c1 ABI short-circuits it
        /// to `&[]`, which a `Config = ()` guest decodes uniformly via
        /// `impl Kind for ()`. The carrier is raw bytes, not a typed
        /// kind, so the substrate stays byte-transparent: the hub /
        /// MCP encode the config struct to bytes at the edge
        /// (SDK-typed, not wire-typed), matching `wasm`'s `Vec<u8>`.
        pub config: Vec<u8>,
        /// ADR-0096 / ADR-0138: which exported actor type to instantiate
        /// from a multi-actor module, named by its `Addressable::NAMESPACE`.
        /// `None` loads the module's **default** type — the single type a
        /// single-actor module has, or the `export!(default = A, …)` opt-in
        /// on a multi-actor module. A defaultless multi-actor module (a
        /// bare `export!(A, B, …)`) has no default, so `None` against it is a
        /// clean `LoadResult::Err` that names the exports (ADR-0138). An
        /// export that the module doesn't declare is likewise a clean
        /// `LoadResult::Err`.
        pub export: Option<String>,
    }

    /// Reply to `LoadComponent`. `Ok` carries the assigned mailbox id,
    /// the resolved name (so callers that omitted `name` learn the
    /// substrate-defaulted one), and the component's advertised
    /// receive-side capabilities parsed from `aether.kinds.inputs`
    /// (ADR-0033). `Err` carries the failure reason — kind-descriptor
    /// conflict, invalid WASM, name conflict, etc.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.load_result")]
    pub enum LoadResult {
        Ok { mailbox_id: aether_data::MailboxId, name: String, capabilities: ComponentCapabilities },
        Err { error: String },
    }

    /// ADR-0033 receive-side capability surface for a component. Built
    /// from the `aether.kinds.inputs` wasm custom section at load time;
    /// the substrate extracts the structured handler / fallback /
    /// component-doc records from the raw section bytes and packs them
    /// into this shape so the hub can store and the MCP harness can
    /// render without a second parser. Empty `handlers` + `None`
    /// fallback + `None` doc describes a component that shipped
    /// without the `#[actor]` macro (ADR-0027 shape) — the hub can
    /// tell those apart from a truly empty receive surface.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    pub struct ComponentCapabilities {
        pub handlers: Vec<HandlerCapability>,
        pub fallback: Option<FallbackCapability>,
        pub doc: Option<String>,
        /// ADR-0090 (issue 1257): the kind the component expects as its
        /// boot config, when it declared a `type Config` other than the
        /// synthesized `()`. `None` for a no-config component. The
        /// capability carries the config kind's id + name; its full
        /// schema is reachable through the engine registry /
        /// `describe_kinds` because the `#[actor]` macro emits a
        /// retention static for the config kind on load, exactly as for
        /// handler kinds.
        pub config: Option<ConfigCapability>,
    }

    /// One `#[handler]` method's advertised capability. `id` is the
    /// compile-time `<K as Kind>::ID` (ADR-0030); `name` is `K::NAME`;
    /// `doc` carries the author's rustdoc filtered through the
    /// `# Agent` section convention when present, else the full doc.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct HandlerCapability {
        pub id: aether_data::KindId,
        pub name: String,
        pub doc: Option<String>,
        /// ADR-0112 / ADR-0134: the handler's reply class — `None` / `One(R)`
        /// for a single-class handler (the ADR-0109 return-type contract),
        /// `Manual` for a manual-class handler that replies by hand,
        /// `Multi(R)` for a multi-class handler emitting 0..n `R` mails. Lets
        /// `describe_component` report the real `In -> Out` so a caller reads
        /// what a call returns before issuing it. Native chassis caps report
        /// `None` until the native handler manifest lands (ADR-0109 §5, a
        /// follow-on).
        pub reply: aether_data::ReplyContract,
    }

    /// A `#[fallback]` method's advertised presence + optional doc.
    /// Components without a fallback are strict receivers; absence of
    /// this field on `ComponentCapabilities` means "no catchall — mail
    /// for unhandled kinds will land as `DISPATCH_UNKNOWN_KIND`".
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct FallbackCapability {
        pub doc: Option<String>,
    }

    /// ADR-0090 (issue 1257) the component's declared boot-config kind.
    /// `id` is the compile-time `<C::Config as Kind>::ID`; `name` is
    /// `C::Config::NAME`. Present only when the component declared a
    /// `type Config` other than the synthesized `()` — a no-config
    /// component leaves `ComponentCapabilities.config` `None`. The
    /// kind's full schema rides the `aether.kinds` section (the macro's
    /// retention static), so `describe_kinds` resolves it by id.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct ConfigCapability {
        pub id: aether_data::KindId,
        pub name: String,
    }

    /// `aether.component.drop` — remove a component from the
    /// substrate and invalidate its mailbox id. Reply: `DropResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.drop")]
    pub struct DropComponent {
        pub mailbox_id: aether_data::MailboxId,
    }

    /// Reply to `DropComponent`. `Ok` on success; `Err` if the
    /// mailbox was unknown, wasn't a component, or already dropped.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.drop_result")]
    pub enum DropResult {
        Ok,
        Err { error: String },
    }

    /// `aether.component.replace` — atomically rebind a target
    /// mailbox id to a freshly instantiated component. ADR-0022: the
    /// substrate freezes the target, drains in-flight mail through
    /// the old instance, then swaps. If the drain exceeds
    /// `drain_timeout_ms` (default 5000) the replace fails with
    /// `ReplaceResult::Err` and the old instance stays bound. Kind
    /// vocabulary rides in the wasm's `aether.kinds` custom section
    /// (ADR-0028). Reply: `ReplaceResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.replace")]
    pub struct ReplaceComponent {
        pub mailbox_id: aether_data::MailboxId,
        pub wasm: Vec<u8>,
        pub drain_timeout_ms: Option<u32>,
        /// ADR-0090 (issue 1257): optional init-config bytes for the
        /// replacement instance, threaded through to its typed `init`
        /// the same way [`LoadComponent::config`] is on first load. An
        /// empty vec means "no config".
        pub config: Vec<u8>,
        /// ADR-0096: which exported actor type to instantiate from the
        /// replacement module, named by its `Addressable::NAMESPACE`. `None`
        /// reuses the trampoline's **current hosted type** (not
        /// necessarily the default), so a bare replace preserves
        /// today's behaviour byte-for-byte. `Some(ns)` instantiates the
        /// named export — mirroring [`LoadComponent::export`] — and an
        /// export the replacement module doesn't declare is a clean
        /// `ReplaceResult::Err`.
        pub export: Option<String>,
    }

    /// Reply to `ReplaceComponent`. Carries the new component's
    /// advertised capabilities on `Ok` so the hub's cached state
    /// reflects the swapped binary; `Err` carries a free-form reason.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.replace_result")]
    pub enum ReplaceResult {
        Ok { capabilities: ComponentCapabilities },
        Err { error: String },
    }

    /// `aether.component.list` — enumerate the components an engine has
    /// actually loaded and registered, addressed to its `aether.component`
    /// mailbox (issue 2020). Fieldless: the query is a definitive snapshot
    /// of the live trampoline set, the only part of the registry whose
    /// membership varies (chassis caps are boot-present and static). A
    /// consumer that spawned an engine with a boot-manifest autoload
    /// (ADR-0116) polls this to learn deterministically when a requested
    /// component is loaded and registered at its lineage address, instead
    /// of inferring liveness by proxy. Reply: `ListComponentsResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
    #[kind(name = "aether.component.list")]
    pub struct ListComponents {}

    /// Reply to `ListComponents` (issue 2020): the ADR-0099 lineage name of
    /// every currently-loaded component (each registered at
    /// `aether.component/<name>`). `names` only — no mailbox id: the id is a
    /// deterministic hash-chain over the lineage the `name` already renders
    /// (ADR-0099), and routing is the substrate's job (a caller addresses by
    /// `recipient_name` and the substrate resolves it), so the handle has no
    /// use at the caller.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.list_result")]
    pub struct ListComponentsResult {
        pub names: Vec<String>,
    }

    /// `aether.component.describe` — introspect one loaded component's
    /// ADR-0033 receive-side `ComponentCapabilities` (handler kinds, docs,
    /// fallback, config kind), addressed to its `aether.component` mailbox
    /// by lineage `name` (the `aether.component/<name>` address that
    /// `ListComponents` / `LoadResult.name` hand back; iamacoffeepot/aether#2421).
    /// Name-addressed because a boot-manifest-loaded component never returns
    /// a mailbox id to its spawner — the substrate is the only process that
    /// always holds the live loaded set, so it owns the answer. Reply:
    /// `DescribeComponentResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.describe")]
    pub struct DescribeComponent {
        /// The component's ADR-0099 lineage name (e.g.
        /// `aether.embedded:aether.camera`), as returned by
        /// `ListComponentsResult.names` or `LoadResult.name`.
        pub name: String,
    }

    /// Reply to `DescribeComponent` (iamacoffeepot/aether#2421): the full
    /// `ComponentCapabilities` on `Ok`, or a free-form reason on `Err` (no
    /// component registered at that lineage name).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.component.describe_result")]
    pub enum DescribeComponentResult {
        Ok { capabilities: ComponentCapabilities },
        Err { error: String },
    }

    /// Reference-image comparison for a `CaptureFrame.similarity` request
    /// (iamacoffeepot/aether#1780). The capture handler reads the PNG at
    /// `reference_path` from the `namespace` assets directory before
    /// dispatching to the render thread; the render thread scores the
    /// captured RGBA against the decoded reference and returns
    /// `similarity_score` + `similarity_pass` on
    /// `CaptureFrameResult::Ok`. Only the `"assets"` namespace is
    /// supported in v1. `threshold` is the maximum normalised MAE
    /// `[0.0, 1.0]` that still counts as a match (`0` = identical only;
    /// `1` = any frame passes); `similarity_pass` is `true` when
    /// `similarity_score <= threshold`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct SimilarityCheck {
        pub namespace: String,
        pub reference_path: String,
        /// Maximum normalised MAE `[0.0, 1.0]` that counts as a match.
        pub threshold: f32,
    }

    /// `aether.render.capture_frame` — request the substrate grab the
    /// current frame contents and reply-to-sender with an encoded
    /// PNG. Carries two optional bundles: `mails` dispatched *before*
    /// capturing (state-changing mail whose effects should appear in
    /// the frame) and `after_mails` dispatched *after* the readback
    /// completes (cleanup, e.g. restoring a flag the caller flipped
    /// for the capture). Both bundles plus the capture land in one
    /// atomic tool call. The render thread's existing mail-drain
    /// barrier before the capture ensures every `mails` entry has
    /// been fully processed by the time the frame is read back.
    /// Empty vecs mean "just capture the current state" /
    /// "no cleanup".
    ///
    /// Abort-on-first-failure policy: if *any* envelope in *either*
    /// bundle fails to resolve (unknown kind or recipient), no mail
    /// is dispatched and the reply is `CaptureFrameResult::Err`. The
    /// whole request aborts before touching the queue.
    ///
    /// `checks` requests a substrate-side verdict scored on the exact
    /// RGBA the PNG is built from — the de-padded, swizzled frame the
    /// render thread maps before the PNG encode (ADR-0105 capture path,
    /// iamacoffeepot/aether#1777). Each entry names one
    /// `test_bench::visual` reduction plus its lit/background partition
    /// params; the results ride back on `CaptureFrameResult::Ok.verdict`.
    /// Empty means "PNG only, no verdict" — the prior behaviour.
    ///
    /// `similarity` requests a reference-image MAE comparison scored on
    /// the same raw RGBA (iamacoffeepot/aether#1780). The handler reads
    /// the reference PNG from the assets namespace before dispatching to
    /// the render thread; the render thread runs the comparison and
    /// returns `similarity_score` / `similarity_pass` on
    /// `CaptureFrameResult::Ok`. `None` means "no similarity check".
    ///
    /// Reply: `CaptureFrameResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame")]
    pub struct CaptureFrame {
        pub mails: Vec<NamedMail>,
        pub after_mails: Vec<NamedMail>,
        pub checks: Vec<FrameCheck>,
        /// Optional reference-image similarity check
        /// (iamacoffeepot/aether#1780). `None` means no comparison.
        pub similarity: Option<SimilarityCheck>,
    }

    /// One mail in a `CaptureFrame.mails` bundle. Structurally mirrors
    /// `aether_data::MailFrame` — a pre-encoded payload plus
    /// the name-level addressing the substrate uses to resolve it.
    /// The hub encodes each entry's `payload` via the kind's
    /// descriptor before wrapping it into the bundle, so the
    /// substrate side just pushes `Mail::new(mailbox, kind_id,
    /// payload, count)` directly.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct NamedMail {
        pub recipient_name: String,
        pub kind_name: String,
        pub payload: Vec<u8>,
        pub count: u32,
    }

    /// Reply to `CaptureFrame`. `Ok` carries the PNG bytes for the
    /// captured frame plus an optional [`FrameVerdict`] (present iff the
    /// request carried `checks`) and an optional similarity score (present
    /// iff the request carried `similarity`); `Err` carries a free-form
    /// reason — capture not supported on this surface, map failed, encode
    /// failed, reference image not found / undecodable, or a
    /// bundle-resolution failure aborting before any mail was dispatched.
    ///
    /// `similarity_score` is the normalised MAE in `[0.0, 1.0]`
    /// (0 = identical, 1 = maximally different).
    /// `similarity_pass` is `true` when `similarity_score <=
    /// SimilarityCheck.threshold` (iamacoffeepot/aether#1780).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.render.capture_frame_result")]
    pub enum CaptureFrameResult {
        Ok {
            png: Vec<u8>,
            verdict: Option<FrameVerdict>,
            /// Normalised MAE score `[0.0, 1.0]`; `None` when no
            /// `similarity` was requested.
            similarity_score: Option<f32>,
            /// `true` when `similarity_score <= threshold`; `None` when
            /// no `similarity` was requested.
            similarity_pass: Option<bool>,
        },
        Err {
            error: String,
        },
    }

    /// Build a [`CaptureFrameResult`] from the raw GPU `render_and_capture`
    /// result shape. Every capture handler in `aether-substrate-bundle`
    /// (test-bench inline, in-process bench, desktop driver) needs this
    /// same `Ok((png, verdict, score, pass)) → Ok { … }` /
    /// `Err(error) → Err { error }` flip. `verdict` is `None` when the
    /// request carried no `checks`; `similarity_score` / `similarity_pass`
    /// are `None` when no `similarity` was requested
    /// (iamacoffeepot/aether#1780).
    impl From<Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>> for CaptureFrameResult {
        fn from(result: Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>) -> Self {
            match result {
                Ok((png, verdict, similarity_score, similarity_pass)) => {
                    Self::Ok { png, verdict, similarity_score, similarity_pass }
                }
                Err(error) => Self::Err { error },
            }
        }
    }

    /// One reduction requested in a [`CaptureFrame::checks`] list. The
    /// `reduction` names which `test_bench::visual` check to run;
    /// `tolerance` is the per-channel threshold that partitions pixels
    /// into the lit/background mask the silhouette reductions share; and
    /// `background` pins the reference RGB — `None` falls back to the
    /// frame's top-left pixel, the `differs_from_background` convention.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct FrameCheck {
        pub reduction: FrameReduction,
        pub tolerance: u8,
        pub background: Option<[u8; 3]>,
        /// `None` scores the whole frame (today's behavior). `Some(rect)`
        /// restricts every reduction to the frame-clamped intersection of
        /// `rect`: coverage divides by the clamped region's pixel count
        /// (not the whole-frame pixel count); `centroid` / `bounding_box`
        /// report absolute frame coordinates, not region-relative ones;
        /// and the background reference is unchanged — still the frame's
        /// top-left pixel unless `background` pins one, never the
        /// region's own top-left (which is routinely inside the drawn
        /// geometry). A region that clips to zero pixels (fully out of
        /// bounds, or a degenerate `min > max`) scores the established
        /// empty-mask result rather than erroring.
        pub region: Option<FrameRect>,
    }

    /// Which `test_bench::visual` reduction a [`FrameCheck`] runs. The
    /// names mirror the public reduction functions one-for-one.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FrameReduction {
        /// `not_all_black` — at least one pixel has a non-zero RGB.
        NotAllBlack,
        /// `differs_from_background` — at least one pixel exceeds the
        /// tolerance band around the background reference.
        DiffersFromBackground,
        /// `coverage` — lit fraction of the frame in `[0.0, 1.0]`.
        Coverage,
        /// `centroid` — mean lit-pixel `(x, y)`.
        Centroid,
        /// `bounding_box` — inclusive lit-pixel extent.
        BoundingBox,
    }

    /// Substrate-side verdict over a captured frame: the frame
    /// dimensions plus one [`FrameCheckResult`] per requested reduction,
    /// scored on the exact de-padded RGBA the PNG was encoded from
    /// (iamacoffeepot/aether#1777). Rides on `CaptureFrameResult::Ok`
    /// when the request carried `checks`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct FrameVerdict {
        pub width: u32,
        pub height: u32,
        pub results: Vec<FrameCheckResult>,
    }

    /// Result of one requested reduction. The variant matches the
    /// [`FrameReduction`] requested; the assertion-style checks
    /// (`NotAllBlack` / `DiffersFromBackground`) report `passed` plus a
    /// `detail` failure string (`None` on pass), and the silhouette
    /// reductions echo the `background` they partitioned against
    /// alongside their scalar / coordinate result (`None` when the lit
    /// mask was empty).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum FrameCheckResult {
        NotAllBlack { passed: bool, detail: Option<String> },
        DiffersFromBackground { passed: bool, detail: Option<String> },
        Coverage { background: [u8; 3], fraction: f32 },
        Centroid { background: [u8; 3], centroid: Option<[f32; 2]> },
        BoundingBox { background: [u8; 3], rect: Option<FrameRect> },
    }

    /// Inclusive axis-aligned pixel extent of a lit region — the wire
    /// mirror of `test_bench::visual::Rect`. `min`/`max` are the smallest
    /// and largest lit column (`x`) and row (`y`); a single lit pixel
    /// yields `min == max`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FrameRect {
        pub min_x: u32,
        pub min_y: u32,
        pub max_x: u32,
        pub max_y: u32,
    }

    // ADR-0105 textured-quad render surface. The texture + quad draw
    // kinds (`CreateTexture` / `CreateTextureResult` / `UpdateTexture` /
    // `TexturedQuad` / `DrawTexturedQuads` / `SolidQuad` /
    // `DrawSolidQuads`) moved to `aether_render::kinds`
    // (ADR-0121). The `QuadScale` / `QuadSpace` projection types stay
    // central: the `aether.text.draw` kind below consumes `QuadSpace`,
    // and `aether-kinds` has no dependency on `aether-render`, so
    // moving them would close a cycle — they're sibling-kind-consumed and
    // therefore pinned here.

    /// Screen/framebuffer-pixel clip rectangle applied after projection.
    ///
    /// Render and text draw calls use this as a per-call scissor. The
    /// coordinates are framebuffer pixels regardless of whether the draw's
    /// projection space is screen or world.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct ClipRect {
        pub x: f32,
        pub y: f32,
        pub width: f32,
        pub height: f32,
    }

    /// How a `QuadSpace::World` quad's clip-space scale factor `k`
    /// relates on-screen size to distance (ADR-0105).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum QuadScale {
        /// `k` is a constant derived from `reference_distance`, so the
        /// perspective divide shrinks the quad as the anchor recedes; the
        /// quad's pixel size holds exactly at `reference_distance`. The
        /// above-the-head label mode.
        Distance { reference_distance: f32 },
        /// `k = clip.w`, cancelling the perspective divide for constant
        /// on-screen size regardless of distance.
        Pixels,
    }

    /// Projection a `DrawTexturedQuads` batch draws under (ADR-0105).
    ///
    /// `Screen` quads are window-pixel rects drawn in an overlay pass
    /// after the world pass under an ortho matrix derived from the surface
    /// size, no depth. `World` quads transform only `anchor` through the
    /// camera's `view_proj`, then apply each quad's pixel offsets in clip
    /// space, so the quad faces the camera and never skews; `scale` picks
    /// the distance-vs-size relationship.
    ///
    /// The render cap implements `Screen`; `World` ships in the vocabulary
    /// now but warn-drops at encode until the world-anchor path lands.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub enum QuadSpace {
        Screen,
        World { anchor: [f32; 3], scale: QuadScale },
    }

    // ADR-0105 text surface. The `aether.text` capability composes the
    // textured-quad surface above into glyphs: load a TTF off the hot
    // path under a session-scoped `font_id`, then draw a string every
    // frame in immediate mode. Structured-shaped; `space` reuses
    // `QuadSpace` so a screen-space HUD string and a world-anchored
    // label ride the same discriminant.

    /// One glyph's horizontal advance, in font units (em-square
    /// subdivisions), keyed by the Unicode scalar value (`char as u32`)
    /// it maps to through the font's cmap. Scale to pixels with
    /// `advance_units * size_pixels / units_per_em`. Not a kind on its
    /// own — only addressable inside `FontMetrics.advances`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct GlyphAdvance {
        /// Unicode scalar value (`char as u32`) this advance applies to.
        pub codepoint: u32,
        /// Horizontal advance in font units.
        pub advance_units: f32,
    }

    /// Size-independent metrics for one loaded font (ADR-0105). Every
    /// measure is in font units — the em square's `units_per_em`
    /// subdivisions — so a consumer caches this table once and scales any
    /// measure to a draw size locally with
    /// `value * size_pixels / units_per_em`, the exact linear scaling the
    /// `aether.text` cap applies as it lays a string out. The
    /// per-codepoint `advances` fold the cmap in; a codepoint the font
    /// has no glyph for advances by `default_advance` (the `.notdef`
    /// glyph's advance), matching the draw path. Carried in
    /// `FontMetricsResult::Ok`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct FontMetrics {
        /// Em-square subdivisions — the denominator that turns a
        /// font-unit measure into a fraction of `size_pixels`.
        pub units_per_em: f32,
        /// Highest point glyphs reach above the baseline, in font units.
        pub ascent: f32,
        /// Lowest point glyphs reach below the baseline, in font units
        /// (typically negative).
        pub descent: f32,
        /// Recommended gap between one line's descent and the next line's
        /// ascent, in font units.
        pub line_gap: f32,
        /// Advance for a codepoint the font has no glyph for — the
        /// `.notdef` glyph's advance, in font units.
        pub default_advance: f32,
        /// Per-codepoint horizontal advances in font units, sorted by
        /// `codepoint`, the cmap folded in.
        pub advances: Vec<GlyphAdvance>,
    }

    /// The three window presentation modes. `Windowed` has no fields —
    /// the current size lives on `SetWindowModeResult`.
    /// `FullscreenExclusive` carries the specific video mode; the
    /// substrate matches against the active monitor's supported modes
    /// and fails the request if none matches (loud rather than
    /// silently falling back).
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub enum WindowMode {
        Windowed,
        FullscreenBorderless,
        FullscreenExclusive { width: u32, height: u32, refresh_mhz: u32 },
    }

    /// `aether.window.set_mode` — switch the substrate's
    /// window presentation mode. `width` / `height` apply only when
    /// `mode == Windowed`; fullscreen modes size themselves from the
    /// monitor / requested video mode. Reply carries the new state
    /// so callers don't have to follow up with a `platform_info`
    /// query.
    ///
    /// Fullscreen-exclusive requests fail with `Err` if no
    /// `VideoMode` on the current monitor matches the `(width,
    /// height, refresh_mhz)` triple exactly. Use `platform_info`
    /// first to enumerate supported modes.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_mode")]
    pub struct SetWindowMode {
        pub mode: WindowMode,
        pub width: Option<u32>,
        pub height: Option<u32>,
    }

    /// Reply to `SetWindowMode`. `Ok` carries the resolved state
    /// after the mode change applied; `Err` carries the reason the
    /// request was rejected (unknown video mode, window not ready,
    /// etc.) with no state change.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_mode_result")]
    pub enum SetWindowModeResult {
        Ok { mode: WindowMode, width: u32, height: u32 },
        Err { error: String },
    }

    /// `aether.window.set_title` — update the substrate
    /// window's title at runtime. `winit::Window::set_title` is
    /// infallible on every supported platform, so the desktop reply
    /// always echoes the applied title back on `Ok`. Headless and hub
    /// chassis reply `Err { error: "unsupported on headless..." }`.
    /// Boot-time default comes from `AETHER_WINDOW_TITLE`; unset falls
    /// back to the substrate's name.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_title")]
    pub struct SetWindowTitle {
        pub title: String,
    }

    /// Reply to `SetWindowTitle`. `Ok` echoes the applied title — same
    /// value the caller sent, returned so MCP logs and agent memory
    /// see the resulting state in one place. `Err` is reserved for
    /// chassis that don't own a window (headless, hub) or for a
    /// pre-window-ready request.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.set_title_result")]
    pub enum SetWindowTitleResult {
        Ok { title: String },
        Err { error: String },
    }

    /// `aether.window.focus` — bring the substrate window to the
    /// foreground (un-minimize, show if hidden, raise + focus). Takes
    /// no fields: focus is a single imperative with no parameters.
    ///
    /// Motivating use (iamacoffeepot/aether#1318): an MCP-driven
    /// session that wants to `capture_frame` against a backgrounded /
    /// minimized / hidden window has no programmatic lever to raise it
    /// otherwise. The desktop driver applies `set_minimized(false)` +
    /// `set_visible(true)` + `focus_window()`. Headless / hub chassis
    /// reply `Err` (no window peripheral).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.focus")]
    pub struct FocusWindow {}

    /// Reply to `FocusWindow`. `Ok` confirms the window was raised
    /// (winit's `focus_window` is best-effort per the platform docs,
    /// but the substrate has applied the three calls). `Err` carries
    /// the reason — a pre-window-ready request, or a chassis without a
    /// window peripheral (headless, hub).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.window.focus_result")]
    pub enum FocusWindowResult {
        Ok,
        Err { error: String },
    }

    // ADR-0088 §6 reverse-lookup inventory actor. The `aether.inventory`
    // mailbox serves the per-build reverse-lookup inventory over mail so
    // an out-of-process observer (the MCP harness) reads the running
    // substrate's *own* inventory instead of a drift-prone compiled-in
    // copy. Two request kinds:
    //
    //   - `aether.inventory.manifest` → the compile-time manifest: every
    //     declared `NameEntry` + every instanced-family `TemplateEntry`.
    //     Templates keep their *family shape* (the client expands a
    //     `Bounded` range / `Declared` domain itself); the manifest does
    //     NOT flatten to a hash → name map (ADR-0088 §6).
    //   - `aether.inventory.resolve { ids }` → per-id `Option<String>`,
    //     the dynamic-instance arm of the resolve chain (ADR-0088 §5) the
    //     client can't compute from the manifest alone.
    //
    // The link-time `aether_data::name_inventory::{NameEntry,
    // TemplateEntry, ParamKind}` are `&'static` (not wire types), so the
    // shapes below are owned, schema-hashed mirrors. `domain` rides as
    // raw bytes (the byte-domain prefix an id is hashed under, e.g.
    // `MAILBOX_DOMAIN` / `THREAD_DOMAIN`) so the client recomputes hashes
    // exactly without depending on the substrate's domain consts.

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

    /// One kind in a [`ListKindsResult`] (ADR-0091). `id` is the
    /// substrate's authoritative [`KindId`](aether_data::KindId) for the
    /// kind; `name` is its declared `Kind::NAME`; `schema_wire` is
    /// the kind's [`SchemaType`](aether_data::SchemaType) encoded with
    /// the wire format (the wire enum carries the full nominal shape).
    ///
    /// The schema rides as opaque wire bytes rather than a directly
    /// embedded `SchemaType` because `SchemaType` itself has no
    /// `Schema` impl (it *is* the schema vocabulary, not a value in
    /// it); shipping it as `Bytes` keeps `KindDescriptorWire` and the
    /// whole reply derivable via [`aether_data::Schema`] without a
    /// hand-roll, at the cost of one extra `wire::from_bytes` on
    /// the harness side. Cap encodes via `wire::to_vec`
    /// against `descriptor.schema`; client decodes via
    /// `wire::from_bytes`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct KindDescriptorWire {
        pub id: aether_data::KindId,
        pub name: String,
        pub schema_wire: Vec<u8>,
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
    /// §5) and the native analogue of the wasm [`HandlerCapability`].
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

    // Mesh-viewer structured load replies (issue 964). The mesh-viewer
    // component's `aether.kit.mesh.load` was fire-and-forget — failures
    // warn-logged and the prior cache stayed, with no wire signal a
    // scenario harness or MCP `send_mail` caller could read. These two
    // reply kinds give the load path the same structured Ok/Err shape
    // the `aether.fs.*_result` family carries (ADR-0041), echoing the
    // request's `namespace` + `path` for correlation per the
    // explicit-nulls convention (every `Option` addressed, never an
    // absent field).
    //
    // Flat-struct shape (`ok: bool` + `error: Option<String>`) rather
    // than an Ok/Err enum so a caller reads success/failure off one
    // field without matching a variant, and so `warnings` rides along
    // on a successful load (e.g. a clamped sphere subdivision) without
    // forcing a third variant. The diagnostic *content* of `error` /
    // `warnings` is a sibling issue — this kind ships only the shape.

    /// `aether.mesh.load_result` — reply to `aether.kit.mesh.load`
    /// (`aether_kit::mesh::LoadMesh`). Echoes the request's
    /// `namespace` + `path` so the caller correlates the reply to its
    /// source without a pending-op queue — operation identity comes
    /// from the reply kind, target identity from the echoed fields.
    /// `ok` is the single success/failure read; `error` is `Some` iff
    /// `ok` is false (read / utf-8 / parse / mesh / unknown-extension
    /// failure); `warnings` carries non-fatal notes (e.g. an
    /// auto-clamped sphere subdivision) on an otherwise-successful
    /// load. Whole-mesh atomic-replace semantics are preserved: a
    /// failed load leaves the prior cached triangles intact.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.mesh.load_result")]
    pub struct MeshLoadResult {
        pub ok: bool,
        pub namespace: String,
        pub path: String,
        pub error: Option<String>,
        pub warnings: Vec<String>,
    }

    /// `aether.scene.load_result` — reply to a future `aether.scene.load`
    /// (issue 964 ships the reply shape ahead of the multi-instance
    /// scene loader; the wire is the bottleneck its sibling issues fill).
    /// Echoes the request's `namespace` + `path`. Whole-scene
    /// atomic-replace semantics are preserved — `ok` is the overall
    /// verdict, `instances_loaded` counts the instances that landed,
    /// and `instance_errors` maps each failed instance name to its
    /// failure reason so a partial scene is diagnosable per-instance.
    /// `error` carries a whole-scene failure (e.g. the scene file
    /// itself failed to read / parse) distinct from the per-instance
    /// `instance_errors`. `BTreeMap` rather than `HashMap` because
    /// `aether-kinds` is `no_std` + `alloc` and the `Schema` derive
    /// encodes `BTreeMap` as `SchemaType::Map` (it rejects `HashMap`);
    /// the keyed-by-instance-name semantics are identical.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.scene.load_result")]
    pub struct SceneLoadResult {
        pub ok: bool,
        pub namespace: String,
        pub path: String,
        pub error: Option<String>,
        pub instance_errors: BTreeMap<String, String>,
        pub instances_loaded: u32,
        pub warnings: Vec<String>,
    }

    // ADR-0081 per-actor log storage. Each actor owns an
    // `ActorLogRing` (in `aether-actor::log`); one wire kind pair
    // drives the query path:
    //
    // - `LogTail` / `LogTailResult` — per-actor query, every actor
    //   responds via the framework-built-in dispatch arm. The MCP
    //   `actor_logs` tool wraps this for a named mailbox; cross-
    //   actor aggregation (when callers want it) is client-side
    //   composition over the same per-actor surface (filed as
    //   iamacoffeepot/aether#960 for the missing fan-out primitive
    //   if substrate-side aggregation ever becomes worthwhile).
    //
    // `LogBatch` / `LogEvent` (the pre-ADR-0081 flush-hop kinds) and
    // `LogRead` / `LogReadResult` (the issue 776 pull surface that
    // `LogCapability` served) retired alongside `LogCapability`.

    /// One log entry as it appears on the wire when an MCP caller
    /// queries an actor's ring via [`LogTail`] / [`LogTailResult`].
    ///
    /// `level` follows the same `0 = trace .. 4 = error` mapping the
    /// rest of `aether.log.*` uses. `origin` is the `MailboxId` of
    /// the actor whose ring buffered the entry: `None` from the
    /// per-actor framework reply (the responder IS the origin —
    /// stamped at client side if the caller is merging across
    /// actors).
    ///
    /// `sequence` is monotonic *per actor's ring*, starting at 1.
    /// Callers walk a single actor's ring via `LogTail::since`; the
    /// cursor is per-actor.
    ///
    /// Not a `Kind` — only addressable as an element of
    /// `LogTailResult::Ok::entries`.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct LogEntry {
        pub timestamp_unix_ms: u64,
        pub level: u8,
        pub target: String,
        pub message: String,
        pub sequence: u64,
        pub origin: Option<aether_data::MailboxId>,
    }

    /// `aether.log.tail` — query one actor's `ActorLogRing`.
    /// Routed to a specific actor by `MailboxId`; the framework's
    /// dispatch loop services this directly (every native actor and
    /// every wasm trampoline answers without the author writing a
    /// handler). Reply: [`LogTailResult`].
    ///
    /// - `max == 0` resolves to the substrate-default cap (currently
    ///   100) — the reply slice never exceeds `MAX_TAIL_MAX` (1000;
    ///   defined in `aether_actor::log`) even on a full ring.
    /// - `min_level: None` returns every level; `Some(2)` returns
    ///   info and above; same `0..=4` mapping the rest of
    ///   `aether.log.*` uses.
    /// - `since: None` returns from the oldest entry in the ring;
    ///   `Some(n)` returns only entries with `sequence > n`.
    /// - `contains: None` applies no content filter; `Some(s)` returns
    ///   only entries whose message contains `s` (case-sensitive).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.log.tail")]
    pub struct LogTail {
        pub max: u32,
        pub min_level: Option<u8>,
        pub since: Option<u64>,
        pub contains: Option<String>,
    }

    /// Reply to [`LogTail`]. `Ok::entries` slices the responder's
    /// ring matching `(min_level, since, contains)`, ordered oldest-to-newest
    /// (ascending `sequence`). `next_since` is the highest `sequence`
    /// in `entries` (or the caller's `since` echoed back on an empty
    /// reply) — thread it into the next `LogTail::since` for a
    /// stable per-actor cursor. `truncated_before` is set when the
    /// ring evicted entries the caller hadn't seen yet (the lowest
    /// `sequence` still in the ring): callers either accept the gap
    /// or poll more often. `entries[i].origin` is `None` — the
    /// responder IS the origin; client-side merge code stamps it if
    /// aggregating across actors.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.log.tail_result")]
    pub enum LogTailResult {
        Ok { entries: Vec<LogEntry>, next_since: u64, truncated_before: Option<u64> },
        Err { error: String },
    }

    // iamacoffeepot/aether#1128 per-handler execution-cost EWMA dump.
    // Each actor folds `(Finished.t − Received.t)` from the dispatch
    // trace bracket into a per-handler `CostCell` (in `aether-actor`);
    // one wire kind pair drives the read-only diagnostic dump, the
    // sibling of `LogTail` / `trace::TraceTail`. Measure-only — Phase 0
    // of iamacoffeepot/aether#1127's cost-aware recruiter, no scheduling
    // change.

    /// One handler's folded execution-cost row as it appears on the
    /// wire when a caller dumps an actor's cost table via [`CostTail`] /
    /// [`CostTailResult`]. `mean_nanos` / `mad_nanos` are the
    /// fixed-point-nanos EWMA mean and mean-absolute-deviation;
    /// `samples` is the folded-sample count (`0` is the neutral seed —
    /// a handler the actor declares but hasn't run yet). `kind_name` is
    /// the substrate-resolved kind name when known, else `None` (a
    /// component-defined kind the dumping engine can't name).
    ///
    /// Not a `Kind` — only addressable as an element of
    /// [`CostTailResult::Ok::rows`].
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct CostRow {
        pub kind_id: aether_data::KindId,
        pub kind_name: Option<String>,
        pub mean_nanos: u64,
        pub mad_nanos: u64,
        pub samples: u64,
    }

    /// `aether.cost.tail` — dump one actor's per-handler execution-cost
    /// EWMA table (iamacoffeepot/aether#1128). Routed to a specific
    /// actor by `MailboxId`; the framework dispatch loop services it
    /// directly (every native actor and every wasm trampoline answers
    /// without the author writing a handler), the same surface
    /// [`LogTail`] / [`crate::trace::TraceTail`] established. Reply:
    /// [`CostTailResult`].
    ///
    /// - `kind: None` returns every handler row the actor declares;
    ///   `Some(id)` returns only that one handler's row (or an empty
    ///   `rows` if the actor has no such handler).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.cost.tail")]
    pub struct CostTail {
        pub kind: Option<aether_data::KindId>,
    }

    /// Reply to [`CostTail`]. `Ok::rows` is one [`CostRow`] per handler
    /// the responding actor declares (filtered to `CostTail::kind` when
    /// set), in unspecified order. `Err` carries a free-form reason
    /// (the actor had no stamped slots / cost cache — a substrate
    /// invariant violation in practice).
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.cost.tail_result")]
    pub enum CostTailResult {
        Ok { rows: Vec<CostRow> },
        Err { error: String },
    }

    // ADR-0066: camera control kinds (`aether.kit.camera.{create, destroy,
    // set_active, set_mode, orbit.set, topdown.set}` + `OrbitParams` /
    // `TopdownParams` / `ModeInit`) live in the `aether-kit::camera`
    // trunk module. The `aether.view_projection` view_proj sink contract stays
    // in `aether-render` — it's a chassis primitive consumed by the desktop chassis's
    // `aether.render` mailbox (the camera mailbox folded into
    // render per ADR-0074 §Decision 7).
    // The migrated kinds are still wire-compatible (kind names +
    // schemas unchanged); only the source-side home moved.

    // ADR-0066: `aether.kit.mesh.load` moved to the `aether-mesh-viewer`
    // trunk crate; that crate was later folded into `aether-kit`
    // (`aether-kit::mesh`), which is its home now.

    /// `aether.test_bench.advance` — request the test-bench chassis
    /// step the world forward by `ticks` Tick events. Each tick
    /// dispatches a `Tick` mail to every subscriber, drains the
    /// resulting mail to quiescence, and renders one frame. Replies
    /// with `AdvanceResult` once all ticks have completed.
    ///
    /// The test-bench chassis is event-driven (ADR-0067): without
    /// an `advance` request the world doesn't tick at all. Smoke
    /// scripts pair this with `capture_frame` to drive deterministic
    /// "send mail → step N → capture" cycles. Other chassis reply
    /// `Err { error: "unsupported on <kind> chassis" }` so callers
    /// fail fast.
    ///
    /// Reply: `AdvanceResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.test_bench.advance")]
    pub struct Advance {
        pub ticks: u32,
    }

    /// Reply to `Advance`. `Ok` echoes the number of ticks completed
    /// (always equal to the request's `ticks` on the happy path —
    /// the variant is structured so a future partial-completion
    /// outcome can extend it without widening the kind). `Err`
    /// carries a free-form reason: chassis doesn't support advance,
    /// dispatcher wedged mid-advance, etc.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.test_bench.advance_result")]
    pub enum AdvanceResult {
        Ok { ticks_completed: u32 },
        Err { error: String },
    }

    /// One environment variable pair carried in [`Spawn::env`]. Pairs
    /// rather than a `HashMap` because structured wire kinds
    /// don't have a `Schema` impl for tuple element types and a
    /// keyed-collection schema isn't load-bearing here — duplicate
    /// keys aren't expected and last-write-wins matches the env
    /// `HashMap` the hub builds anyway.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    pub struct EnvVar {
        pub key: String,
        pub value: String,
    }

    /// `aether.process.spawn` — request the hub chassis launch a
    /// substrate binary as a child process and return the assigned
    /// engine id once the child completes its `Hello` handshake
    /// (ADR-0078 Phase 1, supersedes ADR-0009 §3 for the post-actor
    /// model spawn path). `binary_path` is the absolute filesystem
    /// path to the substrate binary. `args` and `env` are forwarded
    /// to the child verbatim; the hub also injects `AETHER_HUB_URL`
    /// pointing at its engine listener so the child dials back.
    /// `handshake_timeout_ms` caps how long the hub waits for the
    /// child's `Hello` before declaring the spawn failed (default
    /// 5000 ms when `None`). Reply: `SpawnResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.spawn")]
    pub struct Spawn {
        pub binary_path: String,
        pub args: Vec<String>,
        pub env: Vec<EnvVar>,
        pub handshake_timeout_ms: Option<u32>,
    }

    /// Reply to `Spawn`. `Ok` carries the freshly minted engine id
    /// in tagged-string form (`eng-...` per ADR-0064 — `EngineId`
    /// doesn't implement `Schema`, so the wire carries the
    /// authoritative string the substrate registry already uses
    /// at the MCP boundary). The hub adopted the child into its
    /// registry; lifetime is tied to the connection until `Terminate`
    /// or external exit. `Err` carries a free-form reason — io
    /// failure, missing pid, handshake timeout.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.spawn_result")]
    pub enum SpawnResult {
        Ok { engine_id: String, pid: u32 },
        Err { error: String },
    }

    /// `aether.process.terminate` — request the hub chassis shut down
    /// a previously-spawned substrate. Sends SIGTERM, waits up to
    /// `grace_ms` (default 2000 ms when `None`), then escalates to
    /// SIGKILL if the child is still running. `engine_id` is the
    /// Uuid string form the hub returned from `Spawn`. Reply:
    /// `TerminateResult`.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.terminate")]
    pub struct Terminate {
        pub engine_id: String,
        pub grace_ms: Option<u32>,
    }

    /// Reply to `Terminate`. `Ok` reports the child's exit code (if
    /// the kernel returned one) and whether escalation to SIGKILL
    /// was needed. `Err` is for unknown engine ids, externally-
    /// connected engines the hub didn't spawn, or os-level signal
    /// failure.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.terminate_result")]
    pub enum TerminateResult {
        Ok { exit_code: Option<i32>, sigkilled: bool },
        Err { error: String },
    }

    /// `aether.process.exited` — broadcast emitted by the hub's
    /// reaper when a spawned child terminates (whether via
    /// `Terminate` mail or external exit). Fire-and-forget; lands
    /// on every attached MCP session via `egress_broadcast`. The
    /// reaper task converts `Child::wait` completion into this kind
    /// so any cap or operator that wants to react to "engine X
    /// exited" subscribes to broadcast rather than threading a
    /// callback through `EngineRegistry`. `engine_id` is the
    /// Uuid string form the hub used while the child was alive.
    #[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
    #[kind(name = "aether.process.exited")]
    pub struct ProcessExited {
        pub engine_id: String,
        pub exit_code: Option<i32>,
        pub reason: String,
    }

    // ADR-0050 per-provider content-gen caps. The `aether.anthropic`
    // kinds (Role, Message, AnthropicError, MessagesSend, CliSend,
    // MessagesSendResult, CliSendResult) are owned by the capability and
    // live in `aether_anthropic::kinds` (ADR-0121). `Usage`
    // stays central — it is shared with the `aether.gemini` result kinds.

    /// Token + wall-clock accounting returned on a successful
    /// content-gen completion. Shared across the Anthropic text kinds
    /// (issue 1014) and the Gemini media kinds (issue 1015). The CLI
    /// backend can only report `wall_clock_millis` (the subprocess gives no
    /// token counts), leaving the token / cost fields zero / `None`;
    /// the Messages API and the Gemini APIs populate the rest where the
    /// provider reports them.
    #[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Usage {
        pub input_tokens: u32,
        pub output_tokens: u32,
        pub wall_clock_millis: u32,
        pub cost_micros: Option<u64>,
    }
}
