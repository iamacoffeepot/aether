//! `aether-bloomery-bootstrap` — the environment bootstrap script.
//!
//! On a live Bloomery engine an environment is built from two imported
//! images, the distro userland and the Rust toolchain (ADR-0237 decision 3).
//! Each step needs the previous step's reply, so a static mail bundle cannot
//! do it. [`EnvironmentBootstrap`], loaded with the two image references and the
//! paths of the journal owner and the bundle driver as its config, sends the
//! mail an operator would:
//!
//! 1. `aether.workspace.import` of the base, then of the toolchain;
//! 2. `aether.bloomery.journal.read_head`, for the fence;
//! 3. `aether.bloomery.journal.publish` staging the `environment.merge.input`
//!    that cites both trees;
//! 4. `aether.bloomery.driver.call` of `environment.merge`, in the bundle
//!    bound under the head `workspace-programs`;
//! 5. `aether.bloomery.journal.read_artifact` of the transition's result, the
//!    merged `aether.workspace.environment`;
//! 6. `aether.bloomery.journal.publish` moving the head
//!    `(aether.workspace.environment, <platform>)` to that result.
//!
//! It logs each step, and on the first refusal it logs one error naming the
//! step and stops. It records nothing of its own: a rerun moves the head to
//! the same digest again, which appends one more head-move event.
//!
//! The journal owner and the bundle driver are instanced roots in native-only
//! crates, so the script cannot name their types. It proves both paths once
//! at `wire` with `resolve_path` and keeps the two proofs. Bootstrap is
//! ordinary mail from an ordinary component, which is why it lives in its own
//! throwaway crate rather than in the workspace or the engine.

#![forbid(unsafe_code)]

mod config;
mod phase;

use std::mem;

use aether_actor::{ActorInitError, ErasedActorRef, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_bloomery_kinds::{CallOutcome, PublishResult, ReadArtifact, ReadArtifactResult, ReadHead, ReadHeadResult};
use aether_data::ActorPath;
use aether_workspace::{Import, ImportResult, WorkspaceCapability};

use config::Bootstrap;
pub use config::BootstrapConfig;
use phase::{MergeProgram, Peers, Phase, Run};

/// Builds and publishes one environment from its config's two images, then
/// idles.
pub struct EnvironmentBootstrap {
    config: Bootstrap,
    merge: MergeProgram,
    run: Run,
}

#[actor(depends(WorkspaceCapability))]
impl WasmActor for EnvironmentBootstrap {
    type Config = BootstrapConfig;
    const NAMESPACE: &'static str = "aether.bloomery.bootstrap";

    fn init(config: BootstrapConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { config: config.into_bootstrap()?, merge: MergeProgram::new()?, run: Run::Unwired })
    }

    /// Prove both peers, then import the base. A refused path is logged and
    /// nothing is sent.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let Some(journal) = prove(ctx, &self.config.journal) else {
            self.run = Run::Stopped;
            return;
        };
        let Some(driver) = prove(ctx, &self.config.driver) else {
            self.run = Run::Stopped;
            return;
        };

        tracing::info!(image = self.config.base.as_str(), "importing the base image");
        ctx.send::<WorkspaceCapability>(&Import { image: self.config.base.clone() });
        self.run = Run::Live { peers: Peers { journal, driver }, phase: Phase::ImportingBase };
    }

    /// An import's answer: after the base, import the toolchain; after the
    /// toolchain, read the journal head.
    #[handler::single]
    fn on_import_result(&mut self, ctx: &mut WasmCtx<'_>, result: ImportResult) {
        let Some((peers, phase)) = self.take_live("import_result") else {
            return;
        };
        self.run = match (phase, result) {
            (Phase::ImportingBase, ImportResult::Ok { tree }) => {
                tracing::info!(tree = %tree.digest(), "imported the base; importing the toolchain");
                ctx.send::<WorkspaceCapability>(&Import { image: self.config.toolchain.clone() });
                live(peers, Phase::ImportingToolchain { base: tree })
            }
            (Phase::ImportingToolchain { base }, ImportResult::Ok { tree }) => {
                tracing::info!(tree = %tree.digest(), "imported the toolchain; reading the journal head");
                ctx.send_to(peers.journal, &ReadHead);
                live(peers, Phase::ReadingHead { base, toolchain: tree })
            }
            (Phase::ImportingBase | Phase::ImportingToolchain { .. }, ImportResult::Failed { detail }) => {
                stop("import", detail.as_str())
            }
            (phase, _) => out_of_phase("import_result", &phase),
        };
    }

    /// The journal head: stage the merge input under it.
    #[handler::single]
    fn on_read_head_result(&mut self, ctx: &mut WasmCtx<'_>, result: ReadHeadResult) {
        let Some((peers, phase)) = self.take_live("read_head_result") else {
            return;
        };
        self.run = match (phase, result) {
            (Phase::ReadingHead { base, toolchain }, ReadHeadResult::Ok { head }) => {
                match phase::stage_input(base, toolchain, head) {
                    Ok(publish) => {
                        tracing::info!(head, "staging the merge input");
                        ctx.send_to(peers.journal, &publish);
                        live(peers, Phase::Staging { publish })
                    }
                    Err(error) => stop("stage the merge input", &error.to_string()),
                }
            }
            (Phase::ReadingHead { .. }, ReadHeadResult::Err { message }) => stop("read the journal head", &message),
            (phase, _) => out_of_phase("read_head_result", &phase),
        };
    }

    /// A publish's answer: after staging, call the merge; after the head move,
    /// the bootstrap is done. A fence conflict resends the same publish at the
    /// journal's head.
    #[handler::single]
    fn on_publish_result(&mut self, ctx: &mut WasmCtx<'_>, result: PublishResult) {
        let Some((peers, phase)) = self.take_live("publish_result") else {
            return;
        };
        self.run = match (phase, result) {
            (Phase::Staging { .. }, PublishResult::Committed { artifacts, .. }) => match artifacts.as_slice() {
                [input] => {
                    tracing::info!(input = %input, "staged the merge input; calling environment.merge");
                    ctx.send_to(peers.driver, &self.merge.call(*input));
                    live(peers, Phase::Calling)
                }
                staged => {
                    stop("stage the merge input", &format!("the journal staged {} artifacts, not one", staged.len()))
                }
            },
            (Phase::Moving { publish }, PublishResult::Committed { head, .. }) => {
                for moved in publish.moves() {
                    tracing::info!(
                        platform = moved.head().as_str(),
                        environment = %moved.to(),
                        head,
                        "the environment head moved; bootstrap done",
                    );
                }
                live(peers, Phase::Done)
            }
            (Phase::Staging { publish }, PublishResult::Conflict { actual }) => {
                let publish = phase::refence(&publish, actual);
                ctx.send_to(peers.journal, &publish);
                live(peers, Phase::Staging { publish })
            }
            (Phase::Moving { publish }, PublishResult::Conflict { actual }) => {
                let publish = phase::refence(&publish, actual);
                ctx.send_to(peers.journal, &publish);
                live(peers, Phase::Moving { publish })
            }
            (Phase::Staging { .. } | Phase::Moving { .. }, PublishResult::Err { message }) => stop("publish", &message),
            (phase, _) => out_of_phase("publish_result", &phase),
        };
    }

    /// The merge's outcome: read the environment its transition recorded.
    #[handler::single]
    fn on_call_outcome(&mut self, ctx: &mut WasmCtx<'_>, outcome: CallOutcome) {
        let Some((peers, phase)) = self.take_live("call_outcome") else {
            return;
        };
        self.run = match (phase, outcome) {
            (Phase::Calling, CallOutcome::Transition { seq, transition, .. }) => {
                tracing::info!(seq, result = %transition.result, "environment.merge answered; reading the environment");
                ctx.send_to(peers.journal, &ReadArtifact { digest: transition.result });
                live(peers, Phase::Reading { result: transition.result, seq })
            }
            (Phase::Calling, CallOutcome::Fault { fault, .. }) => {
                stop("environment.merge", &format!("faulted: {:?}", fault.reason))
            }
            (Phase::Calling, CallOutcome::Refused { reason, .. }) => {
                stop("environment.merge", &format!("refused: {reason:?}"))
            }
            (phase, _) => out_of_phase("call_outcome", &phase),
        };
    }

    /// The merged environment: move its platform's head to it.
    #[handler::single]
    fn on_read_artifact_result(&mut self, ctx: &mut WasmCtx<'_>, result: ReadArtifactResult) {
        let Some((peers, phase)) = self.take_live("read_artifact_result") else {
            return;
        };
        self.run = match (phase, result) {
            (Phase::Reading { result, seq }, ReadArtifactResult::Found { artifact }) => {
                let moved = phase::environment(&artifact, result).and_then(|environment| {
                    tracing::info!(platform = environment.platform.as_str(), %result, "moving the environment head");
                    phase::head_move(&environment, result, seq).map_err(|error| error.to_string())
                });
                match moved {
                    Ok(publish) => {
                        ctx.send_to(peers.journal, &publish);
                        live(peers, Phase::Moving { publish })
                    }
                    Err(error) => stop("read the environment", &error),
                }
            }
            (Phase::Reading { .. }, ReadArtifactResult::Missing { digest }) => {
                stop("read the environment", &format!("the journal stores no {digest}"))
            }
            (Phase::Reading { .. }, ReadArtifactResult::Err { message, .. }) => stop("read the environment", &message),
            (phase, _) => out_of_phase("read_artifact_result", &phase),
        };
    }
}

impl EnvironmentBootstrap {
    /// Take the live run out for one reply, or log that `reply` arrived while
    /// the run was not live and keep it as it was.
    fn take_live(&mut self, reply: &str) -> Option<(Peers, Phase)> {
        match mem::replace(&mut self.run, Run::Stopped) {
            Run::Live { peers, phase } => Some((peers, phase)),
            other => {
                tracing::error!(reply, run = ?other, "a reply arrived while the bootstrap is not running");
                self.run = other;
                None
            }
        }
    }
}

/// Prove `path`, or log the refusal naming it.
fn prove<A>(ctx: &WasmCtx<'_, A>, path: &ActorPath) -> Option<ErasedActorRef> {
    ctx.resolve_path(path)
        .inspect_err(
            |error| tracing::error!(path = path.as_str(), %error, "a peer path does not prove; bootstrap stopped"),
        )
        .ok()
}

/// The live run at `phase`.
const fn live(peers: Peers, phase: Phase) -> Run {
    Run::Live { peers, phase }
}

/// Log that `step` was refused with `detail`, and stop.
fn stop(step: &str, detail: &str) -> Run {
    tracing::error!(step, detail, "bootstrap stopped");
    Run::Stopped
}

/// Log that `reply` arrived while the run waited on `phase`, and stop.
fn out_of_phase(reply: &str, phase: &Phase) -> Run {
    tracing::error!(reply, ?phase, "a reply arrived out of phase; bootstrap stopped");
    Run::Stopped
}

aether_actor::export!(default = EnvironmentBootstrap);
