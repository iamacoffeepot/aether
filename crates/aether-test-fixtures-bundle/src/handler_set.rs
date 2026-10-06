//! ADR-0169 handler-set fixture: the one wasm adopter of a `#[handler_set]`
//! that a test loads and mails.
//!
//! [`AnswerSet`] carries two request handlers with default bodies, which reach
//! the adopter's state through the set's accessor. [`HandlerSetAdopter`]
//! adopts the set and overrides one of them: the kept request is answered by
//! delegation on a dispatch miss, the replaced one by the adopter's override
//! of the same trait method, and each reply names the body that ran. The
//! adopter's one local handler reports how many set requests it has answered.
//!
//! The set's marker bridge pastes the handlers' kind types into the adopter's
//! module, which is this one, so the imports below serve both.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, handler_set};
use aether_test_fixtures_kinds::{
    AskKept, AskKeptResult, AskReplaced, AskReplacedResult, CountQuery, CountReport, HANDLER_SET_DEFAULT_BODY,
    HANDLER_SET_OVERRIDE_BODY,
};

#[handler_set]
pub trait AnswerSet {
    /// The adopter's count of set requests answered.
    fn answered(&mut self) -> &mut u32;

    /// Answers from the set's default body; no adopter here overrides it.
    #[handler::request]
    fn on_ask_kept(&mut self, _ctx: &mut WasmCtx<'_>, _ask: AskKept) -> AskKeptResult {
        *self.answered() += 1;
        AskKeptResult { answered_by: HANDLER_SET_DEFAULT_BODY }
    }

    /// The default body [`HandlerSetAdopter`] replaces.
    #[handler::request]
    fn on_ask_replaced(&mut self, _ctx: &mut WasmCtx<'_>, _ask: AskReplaced) -> AskReplacedResult {
        *self.answered() += 1;
        AskReplacedResult { answered_by: HANDLER_SET_DEFAULT_BODY }
    }
}

pub struct HandlerSetAdopter {
    answered: u32,
}

impl AnswerSet for HandlerSetAdopter {
    fn answered(&mut self) -> &mut u32 {
        &mut self.answered
    }

    fn on_ask_replaced(&mut self, _ctx: &mut WasmCtx<'_, Self>, _ask: AskReplaced) -> AskReplacedResult {
        self.answered += 1;
        AskReplacedResult { answered_by: HANDLER_SET_OVERRIDE_BODY }
    }
}

#[actor(root, handler_set(AnswerSet))]
impl WasmActor for HandlerSetAdopter {
    const NAMESPACE: &'static str = "test.handler_set.adopter";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HandlerSetAdopter { answered: 0 })
    }

    /// How many set requests this adopter has answered, by either body.
    #[handler::request]
    fn on_count_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.answered }
    }
}
