//! WASM bundle for the program-root `SubstrateHarness` test.

use std::future;

use aether_actor::export;
use aether_bloomery_kinds::{Digest, Mode, OpaqueBytes, Ref, Refusal, Utf8Text};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Async, Env, Http, Process, Program, Sync, program};
use aether_http::{Fetch, FetchResult, HttpMethod};
use aether_process::{Run, RunResult};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.input")]
struct SummarizeInput {
    text: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.summarize.result")]
struct SummarizeResult {
    text: Ref<Utf8Text>,
}

struct Summarize;

#[program]
impl Program for Summarize {
    const NAME: &'static str = "test.program.summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read cited text and stage a derived summary.";
    type Input = SummarizeInput;
    type Result = SummarizeResult;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let text = env.read_text(input.text).await?;
        Ok(SummarizeResult { text: env.stage_text(&format!("summary:{text}")) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.input")]
struct RefuseInput {
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.refuse.result")]
struct RefuseResult {
    marker: u32,
}

struct Refuse;

#[program]
impl Program for Refuse {
    const NAME: &'static str = "test.program.refuse";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Always refuse the invocation.";
    type Input = RefuseInput;
    type Result = RefuseResult;

    fn run(_input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        Err(Refusal::Refused { reason: Detail::new("refused") })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.fetch_body.input")]
struct FetchBodyInput {
    url: Ref<Utf8Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.fetch_body.result")]
struct FetchBodyResult {
    body: Ref<OpaqueBytes>,
}

struct FetchBody;

#[program]
impl Program for FetchBody {
    const NAME: &'static str = "test.program.fetch_body";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Fetch a URL and stage the response body.";
    type Input = FetchBodyInput;
    type Result = FetchBodyResult;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut http: Http) -> Result<Self::Result, Refusal> {
        let url = env.read_text(input.url).await?;
        match http
            .fetch(Fetch {
                request_id: 1,
                url,
                method: HttpMethod::Get,
                headers: Vec::new(),
                body: Vec::new(),
                timeout_ms: None,
            })
            .await?
        {
            FetchResult::Ok { body, .. } => Ok(FetchBodyResult { body: env.stage_bytes(&body) }),
            FetchResult::Err { error, .. } => Err(Refusal::Refused { reason: Detail::new(format!("{error:?}")) }),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.stall.input")]
struct StallInput {
    marker: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.stall.result")]
struct StallResult {
    marker: u32,
}

struct Stall;

#[program]
impl Program for Stall {
    const NAME: &'static str = "test.program.stall";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Never finish, so a test can crash the engine while the call is in flight.";
    type Input = StallInput;
    type Result = StallResult;

    async fn run(_input: Self::Input, _env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        future::pending().await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.read_uncited.input")]
struct ReadUncitedInput {
    text: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.read_uncited.result")]
struct ReadUncitedResult {
    text: Ref<Utf8Text>,
}

struct ReadUncited;

#[program]
impl Program for ReadUncited {
    const NAME: &'static str = "test.program.read_uncited";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Read text the input names by bare digest, which the closure does not carry.";
    type Input = ReadUncitedInput;
    type Result = ReadUncitedResult;

    async fn run(input: Self::Input, env: &mut Env<Async>) -> Result<Self::Result, Refusal> {
        let text = env.read_text(Ref::from_digest(input.text)).await?;
        Ok(ReadUncitedResult { text: env.stage_text(&format!("fetched:{text}")) })
    }
}

export!(
    public = [Summarize, Refuse, FetchBody, Exec, Stall, ReadUncited],
    generators = [aether_bloomery_bundle::bundle],
);

const _: Summarize = Summarize;
const _: Refuse = Refuse;
const _: FetchBody = FetchBody;
const _: Exec = Exec;
const _: Stall = Stall;
const _: ReadUncited = ReadUncited;
