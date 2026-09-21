//! Two-program WASM bundle for the program-root `SubstrateHarness` test.

use aether_actor::export;
use aether_bloomery_kinds::{Mode, OpaqueBytes, Ref, Refusal, Utf8Text};
use aether_bloomery_program::kinds::Detail;
use aether_bloomery_program::{Async, Env, Http, Program, Sync, program};
use aether_http::{Fetch, FetchResult, HttpMethod};

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

export!(Summarize, Refuse, FetchBody, generators = [aether_bloomery_bundle::bundle]);

const _: Summarize = Summarize;
const _: Refuse = Refuse;
const _: FetchBody = FetchBody;
