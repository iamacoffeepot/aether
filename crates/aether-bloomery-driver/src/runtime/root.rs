//! A loaded bundle's root, typed once per declared role at its spawn reply
//! (ADR-0231 §4's guard cast, ADR-0240 D4).
//!
//! The driver cannot name a bundle root's type: each bundle generates its own.
//! The root arrives untyped, as its spawn reply's stamped sender, and the
//! shell casts it to [`ProgramRoot`] and [`ReactorRoot`] for the roles the
//! bundle declares, keeping the typed references for every later send. A
//! generated root publishes only the roles it declares, so a role is cast
//! only when it is declared, and a declared role the root does not publish
//! fails the load.

use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode};
use aether_bloomery_kinds::{ProgramRoot, ReactorRoot};
use aether_data::ErasedActorPath;
use aether_substrate::actor::native::NativeCtx;

use super::RootRoles;

/// One loaded or adopted bundle's root, typed by each role it declares.
pub(super) struct BundleRoot {
    /// The root as a program runner, when the bundle declares programs.
    pub(super) program: Option<ProtocolRef<ProgramRoot>>,
    /// The root as a reactor host, when the bundle declares reactors.
    pub(super) reactor: Option<ProtocolRef<ReactorRoot>>,
}

impl BundleRoot {
    /// Cast `sender`, the root's spawn reply sender at `path`, to each role
    /// in `roles`. `Err` names the first declared role the root does not
    /// publish.
    pub(super) fn cast<A, M: ReplyMode>(
        ctx: &NativeCtx<'_, A, M>,
        sender: ErasedActorRef,
        roles: RootRoles,
        path: &ErasedActorPath,
    ) -> Result<Self, String> {
        let refused = |role: &str| format!("bundle root {path} does not publish the {role} role its bundle declares");
        let program =
            roles.programs.then(|| ctx.cast::<ProgramRoot>(sender).ok_or_else(|| refused("program"))).transpose()?;
        let reactor =
            roles.reactors.then(|| ctx.cast::<ReactorRoot>(sender).ok_or_else(|| refused("reactor"))).transpose()?;

        Ok(Self { program, reactor })
    }
}
