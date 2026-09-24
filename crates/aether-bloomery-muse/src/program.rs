//! `muse.turn`: one stateless turn is one Sampled run with one fetch.

use aether_bloomery_kinds::Mode;
use aether_bloomery_program::{Async, Env, Program, program};

use crate::input::TurnInput;
use crate::result::TurnResult;
use crate::{request, response};

/// Sends one stateless turn over the responses API and stages the reply.
///
/// Reads every cited item text (the driver's closure walk has injected them,
/// so no read fetches), sends exactly one `Fetch`, and records the reply.
/// It never retries: a retry is a new request the graph decides on.
pub struct MuseTurn;

#[program]
impl Program for MuseTurn {
    const NAME: &'static str = "muse.turn";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Send one stateless Muse turn over the responses API and stage the reply.";
    type Input = TurnInput;
    type Result = TurnResult;

    // `#[program]` re-emits each API type at the `export!` site in lib.rs and writes its own return type,
    // so the signature names `Http` and `Refusal` by crate path.
    async fn run(
        input: Self::Input,
        env: &mut Env<Async>,
        mut http: aether_bloomery_program::Http,
    ) -> Result<Self::Result, aether_bloomery_program::Refusal> {
        let mut texts = Vec::with_capacity(input.items().len());
        for item in input.items() {
            texts.push(env.read_text(item.text()).await?);
        }

        response::record(&mut env, http.fetch(request::fetch(&input, &texts)).await?)
    }
}
