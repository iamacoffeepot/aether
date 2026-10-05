//! `muse.turn.input`: everything one turn sends except the credential.
//!
//! The input plus every artifact its items and offered tools cite is the
//! recorded closure, so the journal holds the whole request. Every field is a validated type, and each
//! validated newtype re-checks on decode.

mod items;
mod limits;
mod tools;

use crate::session::TurnSettings;

pub use items::{
    Reasoning, ReasoningId, ReasoningIdError, Role, ToolOutput, TurnItem, TurnItems, TurnItemsError, check_order,
};
pub use limits::{
    CacheKey, CacheKeyError, Endpoint, EndpointError, InputLimit, InputLimitError, ModelName, ModelNameError,
    OutputBudget, OutputBudgetError, ReasoningEffort,
};
pub use tools::{
    CallId, CallIdError, FunctionName, FunctionNameError, OfferedTool, OfferedTools, OfferedToolsError, ToolCall,
    ToolCalls, ToolCallsError, ToolInput,
};

/// One stateless turn: where it goes, which model answers, the programs it
/// offers as tools, the whole conversation, the output budget, the reasoning
/// effort, the input limit, and the session's cache key.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.turn.input")]
pub struct TurnInput {
    /// The absolute `https://` or `http://` URL of the responses endpoint the
    /// turn posts to.
    endpoint: Endpoint,
    /// The model that answers: 1 to 128 bytes matching `[a-z0-9][a-z0-9._-]*`.
    model: ModelName,
    /// The programs offered to the model as tools, each with the definition
    /// sent for it: at most 128, no program twice. Empty offers none.
    tools: OfferedTools,
    /// The whole conversation in order, from 1 to 4096 items, ending on a user
    /// message or a call output.
    items: TurnItems,
    /// The most output tokens, reasoning included, the turn may produce.
    /// Never zero.
    max_output_tokens: OutputBudget,
    /// How much the model reasons before it answers.
    reasoning: ReasoningEffort,
    /// The most input tokens the turn may have been billed for before its
    /// session rests with [`crate::session::RestReason::ContextFull`]. Never
    /// zero.
    input_limit: InputLimit,
    /// The session's key, sent as `prompt_cache_key`.
    cache_key: CacheKey,
}

impl TurnInput {
    /// The one constructor. Each part already holds its own rules.
    #[must_use]
    pub fn new(settings: TurnSettings, items: TurnItems, cache_key: CacheKey) -> Self {
        let (endpoint, model, tools, max_output_tokens, reasoning, input_limit) = settings.into_parts();
        Self { endpoint, model, tools, items, max_output_tokens, reasoning, input_limit, cache_key }
    }

    /// The URL the turn posts to.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The model that answers.
    #[must_use]
    pub const fn model(&self) -> &ModelName {
        &self.model
    }

    /// The programs offered as tools, in the order sent.
    #[must_use]
    pub fn tools(&self) -> &[OfferedTool] {
        self.tools.as_slice()
    }

    /// The whole conversation, in order.
    #[must_use]
    pub fn items(&self) -> &[TurnItem] {
        self.items.as_slice()
    }

    /// The most output tokens the turn may produce.
    #[must_use]
    pub const fn max_output_tokens(&self) -> OutputBudget {
        self.max_output_tokens
    }

    /// How much the model reasons before it answers.
    #[must_use]
    pub const fn reasoning(&self) -> ReasoningEffort {
        self.reasoning
    }

    /// The most input tokens the turn may have been billed for before its
    /// session rests.
    #[must_use]
    pub const fn input_limit(&self) -> InputLimit {
        self.input_limit
    }

    /// The session's key, sent as `prompt_cache_key`.
    #[must_use]
    pub const fn cache_key(&self) -> &CacheKey {
        &self.cache_key
    }

    /// Every field but the conversation and key: what a session keeps from turn to turn.
    #[must_use]
    pub fn settings(&self) -> TurnSettings {
        TurnSettings::new(
            self.endpoint.clone(),
            self.model.clone(),
            self.tools.clone(),
            self.max_output_tokens,
            self.reasoning,
            self.input_limit,
        )
    }

    /// This input with `items` appended to its conversation and every other
    /// field unchanged, so the conversation it sends begins with exactly the
    /// items this one sent.
    pub(crate) fn append(&self, items: impl IntoIterator<Item = TurnItem>) -> Result<Self, TurnItemsError> {
        let items = TurnItems::new(self.items().iter().cloned().chain(items).collect())?;
        Ok(self.settings().with_items(items, self.cache_key.clone()))
    }
}

#[cfg(test)]
pub mod tests {
    use aether_bloomery_kinds::ProgramName;
    use aether_bloomery_program::{NoBound, ToolSchema, function_name};
    use aether_data::Ref;

    use super::{
        CacheKey, CacheKeyError, CallId, Endpoint, EndpointError, FunctionName, InputLimit, InputLimitError, ModelName,
        ModelNameError, OfferedTool, OfferedTools, OutputBudget, OutputBudgetError, ReasoningEffort, Role, ToolCall,
        ToolOutput, TurnInput, TurnItem, TurnItems, TurnItemsError,
    };
    use crate::result::TurnResult;
    use crate::session::MUSE;

    /// `program` offered with a definition citing its own name and real schemas, bound to nothing.
    pub fn offered_tool(program: ProgramName) -> OfferedTool {
        let schema = |schema: ToolSchema| Ref::of_encoded(&schema).expect("a schema encodes");
        let definition = Ref::of_text(program.as_str());
        let bound = Ref::of_encoded(&NoBound).expect("a bound encodes").erase();
        OfferedTool::new(
            program,
            MUSE,
            definition,
            schema(ToolSchema::of::<TurnInput>()),
            bound,
            schema(ToolSchema::of::<TurnResult>()),
        )
    }

    /// Call `id` to `program`'s function name with arguments `{}` that did not decode.
    pub fn call(id: &str, program: &str) -> ToolCall {
        let name = function_name(&ProgramName::new(program).expect("program")).expect("function name");
        ToolCall::refused(
            CallId::new(id).expect("call id"),
            FunctionName::new(name).expect("name"),
            Ref::of_text("{}"),
            Ref::of_text("refused"),
        )
    }

    #[test]
    fn an_append_keeps_the_prefix_and_refuses_an_orphan_output() {
        // Catches an append that rewrites an earlier item or a setting, which would change the prompt prefix the
        // next turn resends, and one that skips the conversation rules.
        use crate::session::TurnSettings;

        let user = TurnItem::message(Role::User, Ref::of_text("hello"));
        let tool = offered_tool(ProgramName::new("muse.echo").expect("program"));
        let first = TurnInput::new(
            TurnSettings::new(
                Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
                ModelName::new("muse-spark-1.3").expect("model"),
                OfferedTools::new(vec![tool]).expect("tools"),
                OutputBudget::new(64).expect("budget"),
                ReasoningEffort::Low,
                InputLimit::new(u64::MAX).expect("limit"),
            ),
            TurnItems::new(vec![user.clone()]).expect("items"),
            CacheKey::new("session-key").expect("key"),
        );
        let output = |id: &str| TurnItem::CallOutput {
            call_id: CallId::new(id).expect("call id"),
            output: ToolOutput::Refused(Ref::of_text("refused")),
        };
        let appended = [TurnItem::Call(call("a", "muse.echo")), output("a")];

        let next = first.append(appended.clone()).expect("an answered call appends");
        assert_eq!(next.settings(), first.settings());
        assert_eq!(next.cache_key(), first.cache_key());
        assert_eq!(next.items(), [user, appended[0].clone(), appended[1].clone()]);
        assert_eq!(first.append([output("b")]), Err(TurnItemsError::OrphanOutput));
    }

    #[test]
    fn each_input_rule_refuses_and_accepts_its_neighbour() {
        // Catches a rule that refuses a valid neighbour or admits its violation.
        let longest_url = format!("https://{}", "a".repeat(Endpoint::MAX_BYTES - "https://".len()));
        let too_long_url = format!("{longest_url}a");
        let endpoints = [
            (too_long_url.as_str(), EndpointError::TooLong, longest_url.as_str()),
            ("https://a b/v1", EndpointError::BadChar, "https://a%20b/v1"),
            ("https://a/v1\n", EndpointError::BadChar, "https://a/v1"),
            ("ftp://a/v1", EndpointError::NotHttp, "http://a/v1"),
            ("example.test/v1", EndpointError::NotHttp, "https://example.test/v1"),
            ("https://", EndpointError::NoHost, "https://a"),
            ("https:///v1", EndpointError::NoHost, "https://a/v1"),
        ];
        for (reject, error, accept) in endpoints {
            assert_eq!(Endpoint::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(Endpoint::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }

        let longest_model = "a".repeat(ModelName::MAX_BYTES);
        let too_long_model = format!("{longest_model}a");
        let models = [
            ("", ModelNameError::Empty, "a"),
            (too_long_model.as_str(), ModelNameError::TooLong, longest_model.as_str()),
            ("-muse", ModelNameError::BadStart, "0muse"),
            ("Muse-spark", ModelNameError::BadStart, "muse-spark"),
            ("muse-Spark", ModelNameError::BadChar, "muse-spark"),
            ("muse/spark-1.3", ModelNameError::BadChar, "muse-spark-1.3-contributor"),
            ("muse spark", ModelNameError::BadChar, "muse_spark"),
        ];
        for (reject, error, accept) in models {
            assert_eq!(ModelName::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(ModelName::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }

        assert_eq!(OutputBudget::new(0), Err(OutputBudgetError::Zero));
        assert_eq!(OutputBudget::new(1).map(OutputBudget::get), Ok(1));

        assert_eq!(InputLimit::new(0), Err(InputLimitError::Zero));
        assert_eq!(InputLimit::new(u64::MAX).map(InputLimit::get), Ok(u64::MAX));
        let limit = InputLimit::new(900).expect("limit");
        assert!(!limit.reached(899), "one token below the limit has not reached it");
        assert!(limit.reached(900), "the limit itself has reached it");
        assert!(limit.reached(901), "past the limit has reached it");

        let call = |id: &str| call(id, "muse.turn");
        let item = |role| TurnItem::message(role, Ref::of_text("text"));
        let user = item(Role::User);
        let output = |id: &str| TurnItem::CallOutput {
            call_id: CallId::new(id).expect("call id"),
            output: ToolOutput::Refused(Ref::of_text("ok")),
        };
        let items = [
            (Vec::new(), TurnItemsError::Empty, vec![user.clone()]),
            (
                vec![user.clone(); TurnItems::MAX_ITEMS + 1],
                TurnItemsError::TooMany,
                vec![user.clone(); TurnItems::MAX_ITEMS],
            ),
            (
                vec![user.clone(), item(Role::Assistant)],
                TurnItemsError::LastNotUser,
                vec![item(Role::Assistant), user.clone()],
            ),
            (vec![item(Role::Developer)], TurnItemsError::LastNotUser, vec![item(Role::Developer), user.clone()]),
            (
                vec![user.clone(), TurnItem::Call(call("a")), output("b")],
                TurnItemsError::OrphanOutput,
                vec![user.clone(), TurnItem::Call(call("a")), output("a")],
            ),
            (
                vec![user.clone(), output("a"), TurnItem::Call(call("a")), output("a")],
                TurnItemsError::OrphanOutput,
                vec![user.clone(), TurnItem::Call(call("a")), output("a")],
            ),
            (
                vec![user.clone(), TurnItem::Call(call("a")), TurnItem::Call(call("a")), output("a")],
                TurnItemsError::DuplicateCall,
                vec![user, TurnItem::Call(call("a")), TurnItem::Call(call("b")), output("a"), output("b")],
            ),
        ];
        for (reject, error, accept) in items {
            assert_eq!(TurnItems::new(reject), Err(error));
            assert_eq!(TurnItems::new(accept.clone()).expect("accepted neighbour").as_slice(), accept.as_slice());
        }
    }

    #[test]
    fn a_cache_key_refuses_what_the_vendor_would_and_draws_hex() {
        // Catches a key the vendor refuses reaching the wire, and a drawn key
        // that is not the lowercase hex of its bytes.
        let longest_key = "a".repeat(CacheKey::MAX_BYTES);
        let too_long_key = format!("{longest_key}a");
        let keys = [
            ("", CacheKeyError::Empty, "a"),
            (too_long_key.as_str(), CacheKeyError::TooLong, longest_key.as_str()),
            ("-key", CacheKeyError::BadStart, "0key"),
            ("Key", CacheKeyError::BadStart, "key"),
            ("key/Key", CacheKeyError::BadChar, "key-key_1.2"),
            ("key key", CacheKeyError::BadChar, "key_key"),
        ];
        for (reject, error, accept) in keys {
            assert_eq!(CacheKey::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(CacheKey::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
        let drawn = CacheKey::drawn(&[1, 171, 255, 0, 16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192])
            .expect("drawn bytes make a key");
        assert_eq!(drawn.as_str().len(), 32, "16 drawn bytes make a 32-character key");
        assert!(
            drawn.as_str().bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "a drawn key is lowercase hex: {}",
            drawn.as_str()
        );
        assert_eq!(CacheKey::drawn(&[15]).expect("one byte draws").as_str(), "0f");
    }
}
