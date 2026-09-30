//! `muse.turn`: one stateless turn is one Sampled run with one fetch.

use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Async, Env, Http, Program, Refusal, program};

use crate::input::TurnInput;
use crate::result::TurnResult;
use crate::{render, request, response};

/// The `muse.turn` program.
pub struct MuseTurn;

/// Sends one stateless turn over the responses API and stages the reply.
///
/// Reads every artifact the input cites (the driver's closure walk has
/// injected them, so no read fetches): each item's text, or a replayed
/// result and its schema, rendered to JSON; and each offered tool's
/// definition and input schema. Sends exactly one `Fetch`, and records the
/// reply, decoding each call's arguments against its tool's input schema.
/// It never retries and never runs a call: a retry is a new request the
/// graph decides on, and a call is its caller's to run.
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
            texts.push(render::item(&mut env, item).await?);
        }
        let mut definitions = Vec::with_capacity(input.tools().len());
        let mut schemas = Vec::with_capacity(input.tools().len());
        for tool in input.tools() {
            definitions.push(env.read_text(tool.definition()).await?);
            schemas.push(env.read(tool.input()).await?);
        }

        let reply = http.fetch(request::fetch(&input, &texts, &definitions)?).await?;
        response::record(&mut env, input.tools(), &schemas, reply)
    }
}
