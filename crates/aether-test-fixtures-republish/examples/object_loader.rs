//! Issue 7629: a guest that pages a module in from the `objects` file
//! namespace, the way a shipped loader does: it reads a package object by
//! hash, publishes the blob it is answered with, and spawns a type from it.
//!
//! - `ObjectLoader` (`test.object.loader`, root) answers each `ObjectSpawn`
//!   with the `SpawnResult` of the spawn its read and publish lead to, or with
//!   a `SpawnResult::Err` naming the stage that failed. The object's bytes
//!   never enter guest memory: the read's blob is a held value, and the
//!   publish names it by hash.

use aether_actor::{ActorInitError, Held, Pending, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_component::ComponentHostCapability;
use aether_fs::{FsCapability, NamespaceAddr, Read, ReadResult};
use aether_kinds::{Publish, PublishResult, Spawn, SpawnResult};
use aether_test_fixtures_kinds::ObjectSpawn;

/// The file namespace a package object is read from.
const OBJECTS: &str = "objects";

/// Carried across the read: the reply owed to the requester, and the
/// namespace to spawn once the object is published.
#[aether_data::kind(name = "aether.test_fixtures.object_loader_read_context")]
struct ReadContext {
    held: Held<SpawnResult>,
    namespace: String,
}

/// Carried across the publish: the reply owed to the requester, and the
/// namespace to spawn now that it is published.
#[aether_data::kind(name = "aether.test_fixtures.object_loader_publish_context")]
struct PublishContext {
    held: Held<SpawnResult>,
    namespace: String,
}

/// Carried across the spawn: the reply owed to the requester.
#[aether_data::kind(name = "aether.test_fixtures.object_loader_spawn_context")]
struct SpawnContext {
    held: Held<SpawnResult>,
}

/// Holds nothing between requests: each load's state rides its contexts.
pub struct ObjectLoader;

#[actor(root, depends(ComponentHostCapability, FsCapability))]
impl WasmActor for ObjectLoader {
    const NAMESPACE: &'static str = "test.object.loader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ObjectLoader)
    }

    #[handler::request]
    fn on_object_spawn(&mut self, ctx: &mut WasmCtx<'_>, request: ObjectSpawn) -> Pending<SpawnResult> {
        let (pending, held) = ctx.hold::<SpawnResult>();
        let ObjectSpawn { hash, namespace } = request;
        let read = Read { addr: NamespaceAddr::new(OBJECTS, hash) };
        let _ = ctx.send_with_context::<FsCapability>(&read, ReadContext { held, namespace });
        pending
    }

    #[handler::response]
    fn on_read(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult, ReadContext { held, namespace }: ReadContext) {
        match result {
            ReadResult::Ok { bytes, .. } => {
                let publish = Publish { code: bytes, configs: Vec::new() };
                let _ = ctx.send_with_context::<ComponentHostCapability>(&publish, PublishContext { held, namespace });
            }
            ReadResult::Err { error, .. } => {
                held.answer(ctx, &SpawnResult::Err { error: format!("read: {error:?}") });
            }
        }
    }

    #[handler::response]
    fn on_published(
        &mut self,
        ctx: &mut WasmCtx<'_>,
        result: PublishResult,
        PublishContext { held, namespace }: PublishContext,
    ) {
        match result {
            PublishResult::Ok { .. } => {
                let spawn = Spawn { namespace, key: None, parent: None, config: Vec::new() };
                let _ = ctx.send_with_context::<ComponentHostCapability>(&spawn, SpawnContext { held });
            }
            PublishResult::Err { error } => {
                held.answer(ctx, &SpawnResult::Err { error: format!("publish: {error}") });
            }
        }
    }

    #[handler::response]
    fn on_spawned(&mut self, ctx: &mut WasmCtx<'_>, result: SpawnResult, SpawnContext { held }: SpawnContext) {
        held.answer(ctx, &result);
    }
}

aether_actor::export!(public = [ObjectLoader]);
