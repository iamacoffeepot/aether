//! The component-lifecycle verbs `FleetHarness` sends over the wire
//! (ADR-0241 §9): [`publish`](FleetHarness::publish) binds a module's
//! namespaces, [`spawn`](FleetHarness::spawn) asks for an instance of a
//! published type, and [`load`](FleetHarness::load) is the publish-plus-spawn
//! convenience a caller reaches for when it wants both in one round trip.
//! [`component_wasm`](FleetHarness::component_wasm) is the registry-resolve
//! hop a caller starting from a selector, rather than dist stem bytes, shares
//! between a `load` and a `publish`.

use aether_data::{Blob, EngineId, Kind};
use aether_kinds::{
    ComponentCapabilities, ComponentSelector, LoadComponent, LoadResult, Publish, PublishResult, PublishedType,
    ResolveComponentResult, Spawn, SpawnResult,
};

use crate::{FleetHarness, single_reply};

/// The two `LoadResult::Ok` fields a [`load`](FleetHarness::load) exposes:
/// the rendered ADR-0099 lineage `addr` (the recipient of every later mail),
/// and the advertised receive-side `capabilities`.
pub struct Loaded {
    pub addr: String,
    pub capabilities: ComponentCapabilities,
}

impl FleetHarness {
    /// Publish `wasm`'s module with no instance configs (ADR-0241 §3, §9):
    /// bind every namespace it exports, spawning its boot once on a first
    /// publish (ADR-0147), or moving every live instance of its namespaces
    /// as one group on a republish (§7) — identical bytes answer `Ok` with
    /// no swap. Returns each type the module publishes with its advertised
    /// capabilities. Panics on `Err` or an undecodable reply.
    pub fn publish(&mut self, engine: EngineId, wasm: Vec<u8>) -> Vec<PublishedType> {
        let replies =
            self.call(Some(engine), "aether.component", &Publish { code: Blob::from(wasm), configs: Vec::new() });
        let payload = single_reply(&replies, "Publish");
        match PublishResult::decode_from_bytes(&payload) {
            Some(PublishResult::Ok { types }) => types,
            Some(PublishResult::Err { error }) => panic!("publish failed: {error}"),
            None => panic!("undecodable PublishResult"),
        }
    }

    /// Ask for an instance of a published type to exist (ADR-0241 §9): a
    /// live name answers `Live` with that instance, not re-initialised; an
    /// absent name answers `Spawned`, standing the instance up with
    /// `spawn.config`. Returns the decoded `Spawned` or `Live` answer so a
    /// scenario can assert which; panics on `Err` or an undecodable reply.
    pub fn spawn(&mut self, engine: EngineId, spawn: &Spawn) -> SpawnResult {
        let replies = self.call(Some(engine), "aether.component", spawn);
        let payload = single_reply(&replies, "Spawn");
        match SpawnResult::decode_from_bytes(&payload) {
            Some(result @ (SpawnResult::Spawned { .. } | SpawnResult::Live { .. })) => result,
            Some(SpawnResult::Err { error }) => panic!("spawn failed: {error}"),
            None => panic!("undecodable SpawnResult"),
        }
    }

    /// Load a component (the publish-plus-spawn convenience `LoadComponent`
    /// gives a caller that wants both in one round trip): send `load` as
    /// given and return its registered lineage address and advertised
    /// capabilities. Panics on `Err` or an undecodable reply.
    pub fn load(&mut self, engine: EngineId, load: &LoadComponent) -> Loaded {
        let replies = self.call(Some(engine), "aether.component", load);
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { path, capabilities }) => Loaded { addr: path.to_string(), capabilities },
            Some(LoadResult::Err { error }) => panic!("load failed: {error}"),
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Resolve a registry `selector` hub-local (ADR-0116) to its stored wasm
    /// bytes and the `@actor` half of a `module@actor` selector — the shared
    /// hop a caller takes before a [`load`](Self::load) or a
    /// [`publish`](Self::publish) when it starts from a selector instead of
    /// dist stem bytes. Panics on a resolve `Err`.
    pub fn component_wasm(&mut self, selector: &str) -> (Vec<u8>, Option<String>) {
        let resolved = self.resolve_component(ComponentSelector {
            query: Some(selector.to_owned()),
            namespace: None,
            handled_kind: None,
        });
        match resolved {
            ResolveComponentResult::Ok { wasm, export, .. } => (wasm, export),
            ResolveComponentResult::Err { error } => panic!("resolve of selector {selector:?} failed: {error}"),
        }
    }
}
