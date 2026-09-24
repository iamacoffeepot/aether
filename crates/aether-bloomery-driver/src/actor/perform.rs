//! Performing the core's commands: one iterative loop over typed sends.

use aether_actor::{DependsOn, ReplyMode};
use aether_bloomery_kinds::{BUNDLE_NAMESPACE, Digest, StatusQuery};
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_kinds::LoadComponent;
use aether_substrate::actor::native::{DeferredReply, NativeCtx};

use super::BundleDriver;
use crate::{CallerId, Command};

impl BundleDriver {
    /// Perform each [`Command`] in order, then return.
    ///
    /// Journal reads and appends go to the handed-over journal reference, loads go
    /// to the component host under the bundle's digest name, root commands go
    /// to the reference the digest's load reply was stamped with, and answers
    /// and fetch answers release the parked reply. Every send carries its ticket as the request
    /// context, so the reply routes back to the core continuation that issued
    /// it. The head watch rides a fresh chain: the journal parks it until the
    /// head moves, and the chain that happens to re-arm it did not cause the
    /// wait.
    pub(crate) fn perform<M: ReplyMode, A: DependsOn<ComponentHostCapability>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        commands: Vec<Command>,
    ) {
        for command in commands {
            match command {
                Command::ReadEvents { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, &ticket);
                }
                Command::ReadArtifact { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, &ticket);
                }
                Command::ReadClosure { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, &ticket);
                }
                Command::Append { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, &ticket);
                }
                Command::Load { ticket, bundle, wasm } => {
                    self.loading.insert(ticket, bundle);
                    let _ = ctx.send_with_context::<ComponentHostCapability>(
                        &LoadComponent {
                            wasm,
                            name: Some(bundle.to_string()),
                            config: Vec::new(),
                            export: Some(BUNDLE_NAMESPACE.to_owned()),
                        },
                        &ticket,
                    );
                }
                Command::Invoke { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, &ticket),
                Command::WatchHead { ticket, request } => {
                    let _ = ctx.send_detached_to_with_context(self.journal, &request, &ticket);
                }
                Command::Warm { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, &ticket),
                Command::Evaluate { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, &ticket),
                Command::QueryStatus { ticket, bundle } => self.send_to_root(ctx, bundle, &StatusQuery, &ticket),
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
                Command::Fetched { caller, result } => {
                    if let Some(owed) = self.take_parked(caller) {
                        owed.reply(ctx, &result);
                    }
                }
                Command::Abort { reason } => ctx.fatal_abort(reason),
            }
        }
    }

    /// Send `request` to `bundle`'s loaded root with `ticket` as the request
    /// context. The core addresses only a digest it saw load, so a digest
    /// with no kept root is a broken invariant and aborts (ADR-0063).
    fn send_to_root<M: ReplyMode, A, K: Kind, C: Kind>(
        &self,
        ctx: &mut NativeCtx<'_, A, M>,
        bundle: Digest,
        request: &K,
        ticket: &C,
    ) {
        let Some(root) = self.roots.get(&bundle) else {
            ctx.fatal_abort(format!("the core addressed bundle {bundle}, whose root the driver never kept"));
        };
        let _ = ctx.send_to_with_context(root, request, ticket);
    }

    /// Take the parked reply tagged with `caller`, if one is still parked.
    fn take_parked(&mut self, caller: CallerId) -> Option<DeferredReply> {
        self.callers.remove(&caller)
    }
}
