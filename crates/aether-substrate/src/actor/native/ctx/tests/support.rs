//! Fixtures shared across the ctx test siblings: the stub actor the
//! type-level asserts stand on, the resolver-flavoured peers the addressing
//! tests send at, and the cast / request-context kinds they carry.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use aether_actor::{Addressable, CallerScope, CallerScoped, HandlesKind, Manual, Resolve};
use aether_data::{Kind, KindId, MailboxId};

use crate::actor::native::{Dispatch, NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;

/// Hand-rolled `Addressable` impl referenced only by the `_assert_actor_send`
/// type-level check in [`super::handles`]. The struct never gets constructed
/// at runtime — its purpose is to fail to instantiate the assert if
/// `NativeActor` ever loses its `Send + 'static` bound.
#[allow(dead_code)]
pub(super) struct StubActor {
    boots: AtomicU32,
}

impl Addressable for StubActor {
    const NAMESPACE: &'static str = "test.stub";
    type Resolver = aether_actor::One;
}

impl aether_actor::Lifecycle<Self> for StubActor {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): (), _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { boots: AtomicU32::new(0) })
    }
}

impl Dispatch<Self> for StubActor {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, Manual, Self>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

impl NativeActor for StubActor {
    type State = Self;
}

/// Issue 629 / Phase A: handle-export round-trip. Caps publish a
/// handle bundle during `init`; consumers retrieve a clone via
/// `get::<H>()`.
#[derive(Clone)]
pub(super) struct StubHandles {
    pub(super) counter: Arc<AtomicU32>,
}

/// A cast kind that is `Pod` but derives neither `Serialize` nor
/// `Deserialize` — the kind ADR-0100's reply path must accept.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(super) struct CastOnly {
    pub(super) code: u32,
}

impl Kind for CastOnly {
    const NAME: &'static str = "test.cast_only_reply";
    const ID: KindId = KindId(0xDEAD_BEEF_0009_0001);

    fn encode_into_bytes(&self) -> Vec<u8> {
        bytemuck::bytes_of(self).to_vec()
    }
}

impl HandlesKind<CastOnly> for StubActor {}

pub(super) struct EmbeddedPeer;

impl Addressable for EmbeddedPeer {
    const NAMESPACE: &'static str = "test.native.embedded_peer";
    type Resolver = aether_actor::Embedded;
}

impl HandlesKind<CastOnly> for EmbeddedPeer {}

pub(super) struct CurrentKeyedPeer;

impl Addressable for CurrentKeyedPeer {
    const NAMESPACE: &'static str = "test.native.current_keyed_peer";
    type Resolver = aether_actor::Many;
}

impl HandlesKind<CastOnly> for CurrentKeyedPeer {}

pub(super) struct ParentKeyed;

impl Resolve for ParentKeyed {
    type Args<'a> = &'a str;

    fn resolve(caller_carry: u64, namespace: &str, name: &str) -> MailboxId {
        <aether_actor::Many as Resolve>::resolve(caller_carry, namespace, name)
    }
}

impl CallerScoped for ParentKeyed {
    const SCOPE: CallerScope = CallerScope::Parent;
}

pub(super) struct ParentKeyedPeer;

impl Addressable for ParentKeyedPeer {
    const NAMESPACE: &'static str = "test.native.parent_keyed_peer";
    type Resolver = ParentKeyed;
}

impl HandlesKind<CastOnly> for ParentKeyedPeer {}

#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "test.native_request_context")]
pub(super) struct NativeRequestContext {
    pub(super) value: u32,
}
