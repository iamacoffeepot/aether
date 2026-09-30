//! `muse.turn.input`: everything one turn sends except the credential.
//!
//! The input plus every text its items and offered tools cite is the recorded
//! closure, so the journal holds the whole request. Every field is a validated type, and each
//! validated newtype re-checks on decode.

mod items;
mod limits;
mod tools;

pub use items::{Role, TurnItem, TurnItems, TurnItemsError};
pub use limits::{
    Endpoint, EndpointError, ModelName, ModelNameError, OutputBudget, OutputBudgetError, ReasoningEffort,
};
pub use tools::{
    CallId, CallIdError, OfferedTool, OfferedTools, OfferedToolsError, ToolCall, ToolCalls, ToolCallsError,
};

/// One stateless turn: where it goes, which model answers, the programs it
/// offers as tools, the whole conversation, the output budget, and the
/// reasoning effort.
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
}

impl TurnInput {
    /// The one constructor. Each part already holds its own rules.
    #[must_use]
    pub const fn new(
        endpoint: Endpoint,
        model: ModelName,
        tools: OfferedTools,
        items: TurnItems,
        max_output_tokens: OutputBudget,
        reasoning: ReasoningEffort,
    ) -> Self {
        Self { endpoint, model, tools, items, max_output_tokens, reasoning }
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
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref};

    use super::{
        CallId, Endpoint, EndpointError, ModelName, ModelNameError, OutputBudget, OutputBudgetError, Role, ToolCall,
        TurnItem, TurnItems, TurnItemsError,
    };

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

        let call = |id: &str| {
            ToolCall::new(
                CallId::new(id).expect("call id"),
                ProgramName::new("muse.turn").expect("program"),
                Ref::of_text("{}"),
            )
        };
        let item = |role| TurnItem::message(role, Ref::of_text("text"));
        let user = item(Role::User);
        let output =
            |id: &str| TurnItem::CallOutput { call_id: CallId::new(id).expect("call id"), output: Ref::of_text("ok") };
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
}
