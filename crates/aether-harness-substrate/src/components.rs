//! The component verbs of [`SubstrateHarness`]: loading, publishing, and
//! spawning guests through the component host (ADR-0241 §9).
//!
//! Every verb sends its kind to the component host with this harness's
//! session as the reply target, waits for the correlated reply, and then
//! waits for the request's whole causal chain to settle, so each one runs
//! the host's real handlers, returns only after a born guest's `wire` and
//! the route publication it staged have landed, and never waits on a clock. The typed
//! verbs read the reply's stamped sender, the actor that answered, and prove
//! it as the requested type.

use aether_actor::{ActorRef, Addressable, ChildOf, ErasedActorRef, Instanced, Root, Singleton};
use aether_component::ComponentHostCapability;
use aether_data::{Blob, ErasedActorPath, Kind, LoadName};
use aether_kinds::{
    InstanceConfig, ListComponents, ListComponentsResult, LoadComponent, LoadResult, Publish, PublishResult,
    PublishedType, Spawn, SpawnResult,
};
use aether_substrate::{ChassisTarget, EgressEvent};

use crate::{SubstrateHarness, SubstrateHarnessError};

/// The instance a [`SubstrateHarness::spawn_any`] answered with.
#[derive(Debug, Clone)]
pub struct SpawnedActor {
    /// The instance's erased reference, read off the reply's stamped sender.
    /// Type it with [`SubstrateHarness::cast`].
    pub actor: ErasedActorRef,
    /// The instance's canonical actor path.
    pub path: ErasedActorPath,
    /// Whether the instance was already live, so the spawn stood nothing up
    /// (`SpawnResult::Live`), rather than stood up by it
    /// (`SpawnResult::Spawned`).
    pub live: bool,
}

impl SubstrateHarness {
    /// Load the component export `R` and return its proven reference; its
    /// path for diagnostics is [`Self::actor_path`].
    ///
    /// Sets `component.export` to `R::NAMESPACE`, sends the load to the
    /// component host with this harness's session as the reply target, and
    /// types the successful reply's stamped sender — the loaded trampoline,
    /// which answers the load itself — as `R`. It returns once the load's
    /// chain settles, `wire` included, so the inline children `wire` spawned
    /// are live and [`Self::child`] finds them. Needs
    /// [`SubstrateHarnessBuilder::with_component_host`](crate::SubstrateHarnessBuilder::with_component_host).
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Load`] when the host refuses the load or the
    /// reply's sender is not the loaded component; the pump's timeout and
    /// decode errors otherwise.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn load<R: Addressable>(&mut self, mut component: LoadComponent) -> Result<ActorRef<R>, SubstrateHarnessError> {
        component.export = Some(R::NAMESPACE.to_owned());
        let (sender, _) = self.load_any(&component)?;

        self.passive.adopt_load::<R>(sender).map_err(|error| SubstrateHarnessError::Load(error.to_string()))
    }

    /// [`Self::load`] for a component whose actor type the test cannot name,
    /// such as a fixture that ships only as wasm: the loaded actor's erased
    /// reference, read off the reply's stamped sender, and its canonical
    /// lineage path. `component` is sent as given. Type the reference with
    /// [`Self::cast`] against a test-local `#[protocol]` naming the rows the
    /// test sends.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Load`] when the host refuses the load or the
    /// reply carries no sender; the pump's timeout and decode errors
    /// otherwise.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn load_any(
        &mut self,
        component: &LoadComponent,
    ) -> Result<(ErasedActorRef, ErasedActorPath), SubstrateHarnessError> {
        let (payload, sender) = self.request_component_host(component, LoadResult::NAME)?;
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { path, .. }) => sender
                .map(|sender| (sender, path))
                .ok_or_else(|| SubstrateHarnessError::Load("the load reply carried no sender stamp".to_owned())),
            Some(LoadResult::Err { error }) => Err(SubstrateHarnessError::Load(error)),
            None => Err(SubstrateHarnessError::Decode("LoadResult decode failed".to_owned())),
        }
    }

    /// Publish the module `code` (ADR-0241 §3, §9): bind every namespace it
    /// exports to it, or, for a successor of the module that publishes them,
    /// republish every live instance of them as one group (§7). Returns each
    /// namespace the module is bound to, as published. A republish whose
    /// config kind changed names its instances' new configs through
    /// [`Self::publish_configured`].
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Publish`] carrying the host's reason when it
    /// refuses the publish; the pump's timeout and decode errors otherwise.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn publish(&mut self, code: Vec<u8>) -> Result<Vec<PublishedType>, SubstrateHarnessError> {
        self.publish_configured(code, Vec::new())
    }

    /// [`Self::publish`] carrying `configs`: the new init config of each live
    /// instance whose type's config kind the successor changed. Every other
    /// instance keeps its stored config.
    ///
    /// # Errors
    ///
    /// As [`Self::publish`].
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn publish_configured(
        &mut self,
        code: Vec<u8>,
        configs: Vec<InstanceConfig>,
    ) -> Result<Vec<PublishedType>, SubstrateHarnessError> {
        let (payload, _) =
            self.request_component_host(&Publish { code: Blob::from(code), configs }, PublishResult::NAME)?;
        match PublishResult::decode_from_bytes(&payload) {
            Some(PublishResult::Ok { types }) => Ok(types),
            Some(PublishResult::Err { error }) => Err(SubstrateHarnessError::Publish(error)),
            None => Err(SubstrateHarnessError::Decode("PublishResult decode failed".to_owned())),
        }
    }

    /// Spawn the published singleton `R` at the root and return its proven
    /// reference; its path for diagnostics is [`Self::actor_path`].
    /// `R::NAMESPACE` names it — a live instance answers as the requested
    /// one, since the name is the instance.
    ///
    /// # Errors
    ///
    /// As [`Self::spawn_any`], and [`SubstrateHarnessError::Spawn`] when the
    /// reply's sender is not an `R`.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn spawn<R: Root + Singleton>(&mut self) -> Result<ActorRef<R>, SubstrateHarnessError> {
        self.spawn_typed::<R>(&Spawn {
            namespace: R::NAMESPACE.to_owned(),
            key: None,
            parent: None,
            config: Vec::new(),
        })
    }

    /// Spawn the published instanced type `R` at the root, keyed by `key`
    /// (`NS:key`), and return its proven reference; its path for diagnostics
    /// is [`Self::actor_path`].
    ///
    /// # Errors
    ///
    /// As [`Self::spawn`].
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn spawn_keyed<R: Root + Instanced>(&mut self, key: &LoadName) -> Result<ActorRef<R>, SubstrateHarnessError> {
        self.spawn_typed::<R>(&Spawn {
            namespace: R::NAMESPACE.to_owned(),
            key: Some(key.as_str().to_owned()),
            parent: None,
            config: Vec::new(),
        })
    }

    /// Spawn the published instanced type `C` beneath the held `parent`,
    /// keyed by `key` (`parent/NS:key`, ADR-0241 §5), and return its proven
    /// reference; its path for diagnostics is [`Self::actor_path`].
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Spawn`] when the registry retains no path for
    /// `parent`; otherwise as [`Self::spawn`].
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn spawn_child<P, C>(
        &mut self,
        parent: &ActorRef<P>,
        key: &LoadName,
    ) -> Result<ActorRef<C>, SubstrateHarnessError>
    where
        P: Addressable,
        C: ChildOf<P> + Instanced,
    {
        let parent = self.passive.actor_path(parent.erase()).ok_or_else(|| {
            SubstrateHarnessError::Spawn(format!("the parent {} retains no actor path", P::NAMESPACE))
        })?;

        self.spawn_typed::<C>(&Spawn {
            namespace: C::NAMESPACE.to_owned(),
            key: Some(key.as_str().to_owned()),
            parent: Some(parent),
            config: Vec::new(),
        })
    }

    /// Send `spawn` as given and return the instance that answered: its
    /// erased reference read off the reply's stamped sender, its canonical
    /// path, and whether it was already live. The door for a type the test
    /// cannot name, such as a fixture that ships only as wasm, or a spawn
    /// that carries a config.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Spawn`] carrying the host's reason when it
    /// refuses the spawn, or when the reply carries no sender; the pump's
    /// timeout and decode errors otherwise.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn spawn_any(&mut self, spawn: &Spawn) -> Result<SpawnedActor, SubstrateHarnessError> {
        let (payload, sender) = self.request_component_host(spawn, SpawnResult::NAME)?;
        let (path, live) = match SpawnResult::decode_from_bytes(&payload) {
            Some(SpawnResult::Spawned { path, .. }) => (path, false),
            Some(SpawnResult::Live { path, .. }) => (path, true),
            Some(SpawnResult::Err { error }) => return Err(SubstrateHarnessError::Spawn(error)),
            None => return Err(SubstrateHarnessError::Decode("SpawnResult decode failed".to_owned())),
        };

        sender
            .map(|actor| SpawnedActor { actor, path, live })
            .ok_or_else(|| SubstrateHarnessError::Spawn("the spawn reply carried no sender stamp".to_owned()))
    }

    /// The component host's `ListComponents` answer: every loaded guest's
    /// canonical path, read from the publication table (ADR-0241 §3). A test
    /// that asserts no route stands reads it here, since a refused or retired
    /// guest leaves no reference to probe.
    ///
    /// # Errors
    ///
    /// The pump's timeout and decode errors.
    ///
    /// # Panics
    ///
    /// Panics when the harness composed no component host.
    pub fn list_components(&mut self) -> Result<Vec<String>, SubstrateHarnessError> {
        let (payload, _) = self.request_component_host(&ListComponents {}, ListComponentsResult::NAME)?;
        ListComponentsResult::decode_from_bytes(&payload)
            .map(|result| result.names)
            .ok_or_else(|| SubstrateHarnessError::Decode("ListComponentsResult decode failed".to_owned()))
    }

    /// [`Self::spawn_any`], with the answering instance proven as `R`.
    fn spawn_typed<R: Addressable>(&mut self, spawn: &Spawn) -> Result<ActorRef<R>, SubstrateHarnessError> {
        let SpawnedActor { actor, .. } = self.spawn_any(spawn)?;

        self.passive.adopt_load::<R>(actor).map_err(|error| SubstrateHarnessError::Spawn(error.to_string()))
    }

    /// Send `request` to the component host as a tracked root with this
    /// harness's session as the reply target, pump until the reply named
    /// `expected` arrives, then wait for the root's whole chain to settle,
    /// returning the reply's payload and stamped sender. A born guest's
    /// `wire` runs on that chain and the registry owner's publication of the
    /// routes it staged holds it open, so every inline child `wire` spawned
    /// is live when this returns.
    fn request_component_host<K, I>(
        &mut self,
        request: &K,
        expected: &'static str,
    ) -> Result<(Vec<u8>, Option<ErasedActorRef>), SubstrateHarnessError>
    where
        K: Kind,
        ActorRef<ComponentHostCapability>: ChassisTarget<K, I>,
    {
        let host = self.passive.actor_ref::<ComponentHostCapability>();
        let cid = self.fresh_correlation_id();
        let (root, settled) = self.passive.send_tracked(host, request, Some(self.session_reply(cid)));

        let EgressEvent::ToSession { payload, sender, .. } = self.pump_until_event(cid, expected)? else {
            return Err(SubstrateHarnessError::Decode(format!("expected a session-targeted {expected}")));
        };
        self.await_settlement(K::ID, root, &settled)?;
        Ok((payload, sender))
    }
}
