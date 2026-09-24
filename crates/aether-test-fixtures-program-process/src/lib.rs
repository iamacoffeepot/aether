//! WASM bundle with one `Process` program, for the driver scenario
//! `a_bundle_with_a_process_program_is_refused_where_process_is_not_composed`:
//! the bloomery chassis composes no `ProcessCapability`, so the host refuses
//! to load this bundle and the driver records the refusal.

use aether_actor::export;
use aether_bloomery_kinds::{Mode, OpaqueBytes, Ref, Refusal, Utf8Text};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Async, Env, Process, Program, program};
use aether_process::{Run, RunResult};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.exec.input")]
struct ExecInput {
    binary: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.exec.result")]
struct ExecResult {
    stdout: Ref<OpaqueBytes>,
}

struct Exec;

#[program]
impl Program for Exec {
    const NAME: &'static str = "test.program.exec";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run a binary and stage stdout.";
    type Input = ExecInput;
    type Result = ExecResult;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut process: Process) -> Result<Self::Result, Refusal> {
        let binary = env.read_text(input.binary).await?;
        match process
            .run(Run { binary, args: Vec::new(), env: Vec::new(), stdin: Vec::new(), timeout_millis: 0 })
            .await?
        {
            RunResult::Ok { stdout, .. } => Ok(ExecResult { stdout: env.stage_bytes(&stdout) }),
            RunResult::TimedOut { .. } | RunResult::Err { .. } => {
                Err(Refusal::Refused { reason: Detail::new("process run did not complete") })
            }
        }
    }
}

export!(Exec, generators = [aether_bloomery_bundle::bundle]);

const _: Exec = Exec;
