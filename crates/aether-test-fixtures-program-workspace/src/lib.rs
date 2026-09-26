//! WASM bundle with one `Workspace` program, for the bloomery chassis scenario
//! in `crates/aether-chassis-bloomery/tests/workspace.rs`: its run becomes a
//! transition citing the stored step output, and an exhausted or failed run
//! becomes the matching fault.

use std::fmt::Display;

use aether_actor::export;
use aether_bloomery_kinds::{Mode, OpaqueBytes, Ref, Refusal, Tree};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Async, Env, Program, Workspace, program};
use aether_workspace::{Environment, Mounts, Network, Run, Scratch, Step, Steps, ToolName};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.workspace.run.input")]
struct RunInput {
    tree: Ref<Tree>,
    environment: Ref<Environment>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.workspace.run.result")]
struct RunOutput {
    exit_code: Option<i32>,
    stdout: Ref<OpaqueBytes>,
    stderr: Ref<OpaqueBytes>,
    tree: Ref<Tree>,
}

struct RunTool;

#[program]
impl Program for RunTool {
    const NAME: &'static str = "test.program.workspace.run";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run the environment's tool over a tree and cite what it produced.";
    type Input = RunInput;
    type Result = RunOutput;

    async fn run(input: Self::Input, _env: &mut Env<Async>, mut workspace: Workspace) -> Result<Self::Result, Refusal> {
        let step = Step {
            tool: ToolName::new("tool").map_err(refused)?,
            args: vec!["target".to_owned()],
            env: Vec::new(),
            stdin: None,
        };
        let run = Run {
            tree: input.tree,
            environment: input.environment,
            mounts: Mounts::new(Vec::new()).map_err(refused)?,
            steps: Steps::new(vec![step]).map_err(refused)?,
            scratch: Scratch::new(Vec::new()).map_err(refused)?,
            network: Network::Off,
        };
        let outcome = workspace.run(run).await?.map_err(|refusal| Refusal::Refused {
            reason: Detail::new(format!("the workspace refused: {refusal:?}")),
        })?;
        let step = outcome.steps.first().ok_or_else(|| refused("the run answered no steps"))?;
        Ok(RunOutput { exit_code: step.exit_code, stdout: step.stdout, stderr: step.stderr, tree: outcome.tree })
    }
}

/// A request this program could not build, as its refusal.
fn refused(error: impl Display) -> Refusal {
    Refusal::Refused { reason: Detail::new(error.to_string()) }
}

export!(public = [RunTool], generators = [aether_bloomery_bundle::bundle]);

const _: RunTool = RunTool;
