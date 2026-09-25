//! ADR-0237 step-0 spike bundle: `spike.workspace.clippy`, a Sampled program that runs one allowlisted binary
//! (`docker`) with the argv its input names, through `aether.process`, and records the exit code, stdout, and
//! stderr as its result.

use aether_actor::export;
use aether_bloomery_kinds::{Mode, OpaqueBytes, Ref, Refusal, Utf8Text};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Async, Env, Process, Program, program};
use aether_process::{EnvVar, Run, RunResult};

/// What to run: the allowlisted binary name, its argv one argument per line, and the child's whole environment as
/// `KEY=VALUE` lines.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "spike.workspace.clippy.input")]
pub struct ClippyInput {
    pub binary: Ref<Utf8Text>,
    pub argv: Ref<Utf8Text>,
    pub env: Ref<Utf8Text>,
    pub timeout_millis: u32,
}

/// What the run produced. `exit_code` is `None` when the child died by signal or the run timed out.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "spike.workspace.clippy.result")]
pub struct ClippyResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Ref<OpaqueBytes>,
    pub stderr: Ref<OpaqueBytes>,
}

struct Clippy;

#[program]
impl Program for Clippy {
    const NAME: &'static str = "spike.workspace.clippy";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run one containerized cargo clippy through aether.process and record its outcome.";
    type Input = ClippyInput;
    type Result = ClippyResult;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut process: Process) -> Result<Self::Result, Refusal> {
        let binary = env.read_text(input.binary).await?;
        let args = env.read_text(input.argv).await?.lines().map(str::to_owned).collect();
        let child_env = env
            .read_text(input.env)
            .await?
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| EnvVar { key: key.to_owned(), value: value.to_owned() })
            .collect();

        let run = Run { binary, args, env: child_env, stdin: Vec::new(), timeout_millis: input.timeout_millis };
        match process.run(run).await? {
            RunResult::Ok { exit_code, stdout, stderr } => Ok(ClippyResult {
                exit_code,
                timed_out: false,
                stdout: env.stage_bytes(&stdout),
                stderr: env.stage_bytes(&stderr),
            }),
            RunResult::TimedOut { stdout, stderr } => Ok(ClippyResult {
                exit_code: None,
                timed_out: true,
                stdout: env.stage_bytes(&stdout),
                stderr: env.stage_bytes(&stderr),
            }),
            RunResult::Err { error } => Err(Refusal::Refused { reason: Detail::new(format!("{error:?}")) }),
        }
    }
}

export!(public = [Clippy], generators = [aether_bloomery_bundle::bundle]);
