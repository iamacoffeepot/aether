use aether_bloomery_kinds::{Mode, Refusal};
use aether_bloomery_program::{Async, Env, Http, Program, program};
use aether_http::{Fetch, HttpMethod};

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.http.input")]
struct In {
    n: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.program.ui.sampled.http.result")]
struct Out {
    n: u32,
}

struct SampledHttp;

#[program]
impl Program for SampledHttp {
    const NAME: &'static str = "test.program.sampled.http";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Passes because Sampled async run takes Http after env.";
    type Input = In;
    type Result = Out;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut http: Http) -> Result<Self::Result, Refusal> {
        let _ = env;
        let _ = http
            .fetch(Fetch {
                request_id: 0,
                url: "https://example.test".into(),
                method: HttpMethod::Get,
                headers: Vec::new(),
                body: Vec::new(),
                timeout_ms: None,
            })
            .await?;
        Ok(Out { n: input.n })
    }
}

fn main() {}
