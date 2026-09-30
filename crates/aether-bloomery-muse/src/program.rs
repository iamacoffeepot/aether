//! `muse.turn`: one stateless turn is one Sampled run with one fetch.

use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Async, Env, Http, Program, Refusal, program};

use crate::input::TurnInput;
use crate::result::TurnResult;
use crate::{request, response};

/// The `muse.turn` program.
pub struct MuseTurn;

/// Sends one stateless turn over the responses API and stages the reply.
///
/// Reads every cited item text and offered tool definition (the driver's
/// closure walk has injected them, so no read fetches), sends exactly one
/// `Fetch`, and records the reply. It never retries, never decodes a call's
/// arguments, and never runs a call: a retry is a new request the graph
/// decides on, and a call is its caller's to run.
#[program]
impl Program for MuseTurn {
    const NAME: &'static str = "muse.turn";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Send one stateless Muse turn over the responses API and stage the reply.";
    type Input = TurnInput;
    type Result = TurnResult;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut http: Http) -> Result<Self::Result, Refusal> {
        let mut texts = Vec::with_capacity(input.items().len());
        for item in input.items() {
            texts.push(env.read_text(item.text()).await?);
        }
        let mut definitions = Vec::with_capacity(input.tools().len());
        for tool in input.tools() {
            definitions.push(env.read_text(tool.definition()).await?);
        }

        response::record(&mut env, input.tools(), http.fetch(request::fetch(&input, &texts, &definitions)?).await?)
    }
}
