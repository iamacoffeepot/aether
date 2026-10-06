//! Protocol-path fixtures (issue #7501, ADR-0231 §3): a guest that is told
//! "an actor covering `PathPoking` stands at this path", in its config and
//! in mail, and mails what stands there.
//!
//! `PathHolder` takes a [`PathHolderConfig`]. Its `wire` resolves the
//! config's `target` and sends a `Bump` through the `ProtocolRef`, and
//! spawns a `PathHolderChild` with the config's `child_target`; either
//! refusal fails the birth with the reason. A [`PathAttach`] request carries
//! a path of its own: the holder resolves it and answers `Ok` naming it, or
//! the refusal. The last attached path is the holder's saved state, so a
//! republish decodes one from saved-state bytes. A [`PathEcho`] reads both
//! paths back.
//!
//! `PathHolderChild` is the holder's inline child. It keeps the path its
//! config named and echoes it at its alias.
//!
//! The target either names is any guest that tells on `Bump`, such as
//! `ParentPeerStandIn`.

use aether_actor::{
    ActorInitError, PathRefused, ProtocolPath, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor,
};
use aether_data::ErasedActorPath;
use aether_test_fixtures_kinds::{
    Bump, PATH_HOLDER_CHILD, PathAnswer, PathAttach, PathEcho, PathEchoed, PathHolderConfig, PathPoking,
};

/// The holder's durable state: the path its last accepted `PathAttach`
/// named.
#[aether_data::kind(name = "aether.test_fixtures.path_holder_state", no_serde)]
pub struct PathHolderState {
    attached: Option<ProtocolPath<PathPoking>>,
}

pub struct PathHolder {
    config: PathHolderConfig,
    attached: Option<ProtocolPath<PathPoking>>,
}

#[actor(instanced, root, spawns(PathHolderChild))]
impl WasmActor for PathHolder {
    type Config = PathHolderConfig;
    type State = PathHolderState;
    const NAMESPACE: &'static str = "test.protocol_path.holder";

    fn init(config: PathHolderConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(PathHolder { config, attached: None })
    }

    /// Resolve the config's target and send a `Bump` through the reference,
    /// then spawn the child with its own path. A republish rebuilds the
    /// child before this runs, so a resident child is left as it is.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        if let Some(target) = &self.config.target {
            let reference = ctx.resolve(target).map_err(|error| ActorInitError::new(error.to_string()))?;
            ctx.send_to(reference, &Bump);
        }

        let Some(child_target) = &self.config.child_target else {
            return Ok(());
        };
        if ctx.child_as::<PathHolderChild>(PATH_HOLDER_CHILD).is_some() {
            return Ok(());
        }
        let config = PathHolderConfig { target: Some(child_target.clone()), child_target: None };

        ctx.spawn_inline::<PathHolderChild>(Subname::Named(PATH_HOLDER_CHILD), &config)
            .map(drop)
            .map_err(|error| ActorInitError::new(format!("the child was refused: {error:?}")))
    }

    fn dehydrate(&self) -> PathHolderState {
        PathHolderState { attached: self.attached.clone() }
    }

    fn rehydrate(&mut self, state: PathHolderState) {
        self.attached = state.attached;
    }

    /// Keep the attached path when a live actor stands at it, and say so;
    /// answer the refusal when none does.
    #[handler::request]
    fn on_attach(&mut self, ctx: &mut WasmCtx<'_>, attach: PathAttach) -> PathAnswer {
        match ctx.resolve(&attach.target) {
            Ok(_) => {
                let path = attach.target.as_erased().clone();
                self.attached = Some(attach.target);
                PathAnswer::Ok { path }
            }
            Err(error) => PathAnswer::Err(PathRefused::from(error)),
        }
    }

    #[handler::request]
    fn on_echo(&mut self, _ctx: &mut WasmCtx<'_>, _echo: PathEcho) -> PathEchoed {
        PathEchoed { config: self.config.target.as_ref().map(erased), attached: self.attached.as_ref().map(erased) }
    }
}

/// The holder's inline child: it keeps the path its config named.
pub struct PathHolderChild {
    config: PathHolderConfig,
}

#[actor(instanced, child_of(PathHolder))]
impl WasmActor for PathHolderChild {
    type Config = PathHolderConfig;
    const NAMESPACE: &'static str = "test.protocol_path.holder_child";

    fn init(config: PathHolderConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(PathHolderChild { config })
    }

    #[handler::request]
    fn on_echo(&mut self, _ctx: &mut WasmCtx<'_>, _echo: PathEcho) -> PathEchoed {
        PathEchoed { config: self.config.target.as_ref().map(erased), attached: None }
    }
}

/// The text of a held path, for an echo.
fn erased(path: &ProtocolPath<PathPoking>) -> ErasedActorPath {
    path.as_erased().clone()
}
