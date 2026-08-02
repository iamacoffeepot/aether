//! Shared fixtures and macros for the chassis-builder tests: pod kind
//! declarations, close-observing actor shapes, the stub passive cap, and the
//! driver-carrying test chassis.

use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::builder::{BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError};
use crate::mail::KindId;
use crate::{BootError, Chassis, NativeActor, NativeInitCtx};
use aether_actor::Addressable;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

macro_rules! pod_kind {
    ($type:ident { $field:ident: $field_ty:ty }, $name:literal, $id:expr) => {
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct $type {
            $field: $field_ty,
        }

        impl aether_data::Kind for $type {
            const NAME: &'static str = $name;
            const ID: aether_data::KindId = aether_data::KindId($id);

            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != std::mem::size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }

            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }
    };
}

macro_rules! close_observed_state {
    ($type:ident, $namespace:literal) => {
        struct $type {
            close_observed: Arc<AtomicU32>,
        }

        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::Many;
        }

        impl aether_actor::Root for $type {}

        impl aether_actor::Lifecycle<Self> for $type {
            type Config = ();
            type Params = Arc<AtomicU32>;
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;

            fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { close_observed: params })
            }

            fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
                state.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }

        impl NativeActor for $type {
            type State = Self;
        }
    };
}

macro_rules! close_observed_actor {
    ($type:ident, $namespace:literal) => {
        close_observed_state!($type, $namespace);

        impl Dispatch<Self> for $type {
            fn dispatch(
                _state: &mut Self,
                _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
                _kind: KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }
    };
}

macro_rules! shutdown_on_kind_actor {
    ($type:ident, $namespace:literal, $kind:ty) => {
        close_observed_state!($type, $namespace);

        impl HandlesKind<$kind> for $type {}

        shutdown_dispatch!($type, $kind);
    };
}

macro_rules! shutdown_dispatch {
    ($type:ident, $kind:ty) => {
        impl Dispatch<Self> for $type {
            fn dispatch(
                _state: &mut Self,
                ctx: &mut NativeCtx<'_, crate::Manual, Self>,
                kind: KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == <$kind as aether_data::Kind>::ID.0 {
                    let _ = <$kind as aether_data::Kind>::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }
    };
}

macro_rules! unit_shutdown_actor {
    ($type:ident, $namespace:literal, $kind:ty) => {
        struct $type;

        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::Many;
        }

        impl aether_actor::Root for $type {}

        impl HandlesKind<$kind> for $type {}

        impl aether_actor::Lifecycle<Self> for $type {
            type Config = ();
            type Params = ();
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;

            fn init((): Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }

        impl NativeActor for $type {
            type State = Self;
        }

        shutdown_dispatch!($type, $kind);
    };
}

/// Lightweight passive-cap fixture for chassis-level boot tests.
/// The chassis-builder tests don't care about handler dispatch
/// (per-cap dispatch coverage lives in the per-cap crates); the
/// real caps would force a circular dep, so this stub stands in.
pub(super) struct StubLog;
impl Addressable for StubLog {
    const NAMESPACE: &'static str = "test.chassis_builder.stub_log";
    type Resolver = aether_actor::One;
}
impl aether_actor::Root for StubLog {}

impl aether_actor::Lifecycle<Self> for StubLog {
    type Config = ();
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}

impl NativeActor for StubLog {
    type State = Self;
}

impl Dispatch<Self> for StubLog {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, crate::Manual, Self>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

/// Fixture chassis for driver-build tests. Generic over the
/// concrete `DriverCapability` so each test can pair the chassis
/// type with whatever driver it's exercising.
pub(super) struct DrivenTestChassis<D: DriverCapability>(PhantomData<fn() -> D>);
impl<D: DriverCapability + 'static> Chassis for DrivenTestChassis<D> {
    const PROFILE: &'static str = "test-driven";
    type Driver = D;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("DrivenTestChassis is driven by Builder::new directly in unit tests");
    }
}

/// Test driver: records that it ran, then exits.
pub(super) struct RanDriver {
    pub(super) ran: Arc<AtomicBool>,
}

pub(super) struct RanDriverRunning {
    ran: Arc<AtomicBool>,
}

impl DriverCapability for RanDriver {
    type Running = RanDriverRunning;
    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        Ok(RanDriverRunning { ran: self.ran })
    }
}

impl DriverRunning for RanDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}
