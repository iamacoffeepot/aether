//! Performing the core's commands: one iterative loop over typed sends.

use aether_actor::ReplyMode;
use aether_bloomery_journal::JournalActor;
use aether_bloomery_kinds::{BUNDLE_NAMESPACE, StatusQuery};
use aether_component::ComponentHostCapability;
use aether_kinds::LoadComponent;
use aether_substrate::actor::native::{DeferredReply, NativeCtx};

use super::{BundleDriver, BundleRoot};
use crate::{CallerId, Command};

impl BundleDriver {
    /// Perform each [`Command`] in order, then return.
    ///
    /// Journal reads and appends go to the handed-over journal id, loads go
    /// to the component host under the bundle's digest name, invokes go to
    /// the loaded root's handed-over id, and answers release the parked
    /// reply. Every send carries its ticket as the request context, so the
    /// reply routes back to the core continuation that issued it.
    pub(crate) fn perform<M: ReplyMode, A>(&mut self, ctx: &mut NativeCtx<'_, A, M>, commands: Vec<Command>) {
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
                Command::Load { ticket, bundle, wasm } => {
                    let _ = ctx.erase().actor::<ComponentHostCapability>().with_context(&ticket).send(&LoadComponent {
                        wasm,
                        name: Some(bundle.to_string()),
                        config: Vec::new(),
                        export: Some(BUNDLE_NAMESPACE.to_owned()),
                    });
                }
                Command::Invoke { ticket, root, request } => {
                    let _ = ctx.actor_at::<BundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::WatchHead { ticket, request } => {
                    let _ = ctx.actor_at::<JournalActor>(self.journal).with_context(&ticket).send(&request);
                }
                Command::Warm { ticket, root, request } => {
                    let _ = ctx.actor_at::<BundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::Evaluate { ticket, root, request } => {
                    let _ = ctx.actor_at::<BundleRoot>(root).with_context(&ticket).send(&request);
                }
                Command::QueryStatus { ticket, root } => {
                    let _ = ctx.actor_at::<BundleRoot>(root).with_context(&ticket).send(&StatusQuery);
                }
                Command::Answer { caller, outcome } => {
                    // A second answer for one caller drops: the caller already
                    // holds its exactly-once outcome, so no reply is owed.
                    if let Some(owed) = self.take_parked(caller) {
                        owed.reply(ctx, &outcome);
                    }
                }
                Command::Processed { caller, reply } => {
                    if let Some(owed) = self.take_parked(caller) {
                        owed.reply(ctx, &reply);
                    }
                }
                Command::Abort { reason } => ctx.fatal_abort(reason),
            }
        }
    }

    /// Take the parked reply tagged with `caller`, if one is still parked.
    fn take_parked(&mut self, caller: CallerId) -> Option<DeferredReply> {
        let index = self.callers.iter().position(|(parked, _)| *parked == caller)?;
        Some(self.callers.swap_remove(index).1)
    }
}
