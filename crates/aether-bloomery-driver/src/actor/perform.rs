//! Performing the core's commands: one iterative loop over typed sends.

use aether_actor::ReplyMode;
use aether_bloomery_journal::JournalActor;
use aether_bloomery_kinds::StatusQuery;
use aether_bloomery_program::PROGRAM_NAMESPACE;
use aether_bloomery_reactor::REACTOR_NAMESPACE;
use aether_component::ComponentHostCapability;
use aether_kinds::LoadComponent;
use aether_substrate::actor::native::NativeCtx;

use super::root::ReactorBundleRoot;
use super::{BundleDriver, ProgramBundleRoot};
use crate::{BundleRole, Command};

impl BundleDriver {
    /// Perform each [`Command`] in order, then return.
    ///
    /// Journal reads and appends go to the handed-over journal id, loads go
    /// to the component host under the bundle's digest name, invokes go to
    /// the loaded root's handed-over id, and answers release the parked
    /// reply. Every send carries its ticket as the request context, so the
    /// reply routes back to the core continuation that issued it.
    pub(crate) fn perform<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, M, A>, commands: Vec<Command>) {
        for command in commands {
            match command {
                Command::ReadEvents { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::ReadArtifact { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::ReadClosure { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::Append { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::Load { ticket, bundle, role, wasm } => {
                    let export = match role {
                        BundleRole::Program => PROGRAM_NAMESPACE,
                        BundleRole::Reactor => REACTOR_NAMESPACE,
                    };
                    let _ = ctx.actor::<ComponentHostCapability>().with_context(&ticket).send(&LoadComponent {
                        wasm,
                        name: Some(bundle.to_string()),
                        config: Vec::new(),
                        export: Some(export.to_owned()),
                    });
                }
                Command::Invoke { ticket, root, request } => {
                    let _ = ctx.actor_at::<ProgramBundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::WatchHead { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::Warm { ticket, root, request } => {
                    let _ = ctx.actor_at::<ReactorBundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::Evaluate { ticket, root, request } => {
                    let _ = ctx.actor_at::<ReactorBundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::QueryStatus { ticket, root } => {
                    let _ = ctx.actor_at::<ReactorBundleRoot>(root).with_context(&ticket).send(&StatusQuery);
                }
                Command::Answer { caller, outcome } => {
                    // A second answer for one caller drops: the caller already
                    // holds its exactly-once outcome, so no reply is owed.
                    if let Some(owed) = self.callers.remove(&caller) {
                        owed.reply(ctx, &outcome);
                    }
                }
                Command::Processed { caller, reply } => {
                    if let Some(owed) = self.callers.remove(&caller) {
                        owed.reply(ctx, &reply);
                    }
                }
                Command::Abort { reason } => ctx.fatal_abort(reason),
            }
        }
    }
}
