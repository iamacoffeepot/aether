//! Performing the core's commands: one iterative loop over typed sends.

use aether_actor::{DependsOn, ReplyMode};
use aether_bloomery_kinds::{Digest, StatusQuery};
use aether_bloomery_workspace::Run;
use aether_component::ComponentHostCapability;
use aether_data::{ActorMail, Kind};
use aether_http::HttpCapability;
use aether_kinds::{Publish, Spawn};
use aether_substrate::actor::native::NativeCtx;

use super::{BundleDriverState, Caller, CallerId, Command, LoadTicket};

impl BundleDriverState {
    /// Perform each [`Command`] in order, then return.
    ///
    /// Journal reads and appends go to the handed-over journal reference, a
    /// load publishes the bundle's code to the component host (its reply
    /// spawns the root under the unit key), root commands go to the
    /// reference the digest's spawn reply was stamped with, a program's
    /// relayed `Http` call goes to the http capability and its `Workspace` call
    /// to the held workspace reference, and answers, fetch answers, and API
    /// answers release the held reply. Every send carries its ticket as the request
    /// context, so the reply routes back to the core continuation that issued
    /// it. The head watch rides a fresh chain: the journal parks it until the
    /// head moves, and the chain that happens to re-arm it did not cause the
    /// wait.
    pub(crate) fn perform<M: ReplyMode, A: DependsOn<ComponentHostCapability> + DependsOn<HttpCapability>>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        commands: Vec<Command>,
    ) {
        for command in commands {
            match command {
                Command::ReadEvents { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, ticket);
                }
                Command::ReadArtifact { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, ticket);
                }
                Command::ReadClosure { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, ticket);
                }
                Command::Append { ticket, request } => {
                    let _ = ctx.send_to_with_context(self.journal, &request, ticket);
                }
                Command::Load { ticket, bundle, wasm } => {
                    self.loading.insert(ticket, bundle);
                    let _ = ctx.send_with_context::<ComponentHostCapability>(
                        &Publish { code: wasm.into(), configs: Vec::new() },
                        ticket,
                    );
                }
                Command::Invoke { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, ticket),
                Command::WatchHead { ticket, request } => {
                    let _ = ctx.send_detached_to_with_context(self.journal, &request, ticket);
                }
                Command::Warm { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, ticket),
                Command::Evaluate { ticket, bundle, request } => self.send_to_root(ctx, bundle, &request, ticket),
                Command::QueryStatus { ticket, bundle } => self.send_to_root(ctx, bundle, &StatusQuery, ticket),
                // A second answer for one caller drops: the caller already
                // holds its exactly-once outcome, so no reply is owed.
                Command::Answer { caller, outcome } => match self.callers.remove(&caller) {
                    Some(Caller::Call(held)) => held.answer(ctx, &outcome),
                    Some(_) => owed_other(ctx, caller, "CallOutcome"),
                    None => {}
                },
                Command::Processed { caller, reply } => match self.callers.remove(&caller) {
                    Some(Caller::Processed(held)) => held.answer(ctx, &reply),
                    Some(_) => owed_other(ctx, caller, "Processed"),
                    None => {}
                },
                Command::Fetched { caller, result } => match self.callers.remove(&caller) {
                    Some(Caller::Fetched(held)) => held.answer(ctx, &result),
                    Some(_) => owed_other(ctx, caller, "ReadArtifactResult"),
                    None => {}
                },
                Command::Fetch { ticket, request } => {
                    let _ = ctx.send_with_context::<HttpCapability>(&request, ticket);
                }
                Command::RunWorkspace { ticket, request } => {
                    let run = Run { source: self.source.clone(), request };
                    let _ = ctx.send_to_with_context(self.workspace, &run, ticket);
                }
                Command::ApiAnswered { caller, result } => match self.callers.remove(&caller) {
                    Some(Caller::Api(held)) => held.answer(ctx, &result),
                    Some(_) => owed_other(ctx, caller, "ApiCallResult"),
                    None => {}
                },
                Command::Abort { reason } => ctx.fatal_abort(reason),
            }
        }
    }

    /// Spawn a published bundle's root at its bound `namespace` under the
    /// unit key, with the load's `ticket` as the request context: the second
    /// half of a [`Command::Load`], sent once its publish answers.
    pub(crate) fn spawn_root<M: ReplyMode, A: DependsOn<ComponentHostCapability>>(
        &self,
        ctx: &mut NativeCtx<'_, A, M>,
        namespace: String,
        ticket: LoadTicket,
    ) {
        let spawn = Spawn { namespace, key: Some(self.unit.as_str().to_owned()), parent: None, config: Vec::new() };
        let _ = ctx.send_with_context::<ComponentHostCapability>(&spawn, ticket);
    }

    /// Send `request` to `bundle`'s loaded root with `ticket` as the request
    /// context. The core addresses only a digest it saw load, so a digest
    /// with no kept root is a broken invariant and aborts (ADR-0063).
    fn send_to_root<M: ReplyMode, A, K: ActorMail, C: Kind>(
        &self,
        ctx: &mut NativeCtx<'_, A, M>,
        bundle: Digest,
        request: &K,
        ticket: C,
    ) {
        let Some(root) = self.roots.get(&bundle) else {
            ctx.fatal_abort(format!("the core addressed bundle {bundle}, whose root the driver never kept"));
        };
        let _ = ctx.send_to_with_context(root, request, ticket);
    }
}

/// The core answered `caller` with a `reply` kind that caller's held ticket
/// does not owe: the core addressed the wrong kind of caller, a broken
/// invariant that aborts (ADR-0063). The removed ticket drops silently
/// during the unwind.
fn owed_other<M: ReplyMode, A>(ctx: &NativeCtx<'_, A, M>, caller: CallerId, reply: &str) -> ! {
    ctx.fatal_abort(format!("the core answered caller {caller:?} with a {reply} it does not owe"))
}
