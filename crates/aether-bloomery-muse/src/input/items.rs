//! The conversation a turn resends: a flat, ordered list of role-tagged
//! texts, and the calls and outputs replayed from earlier turns.

use std::collections::BTreeSet;

use aether_bloomery_kinds::{Digest, ErasedRef, Ref, Utf8Text};
use aether_bloomery_program::ToolSchema;

use super::tools::{CallId, ToolCall};

/// Who spoke an item. System-style instructions are a leading `Developer` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub enum Role {
    /// Instructions to the model, above the conversation.
    Developer,
    /// The person the model answers.
    User,
    /// The model's own earlier reply.
    Assistant,
}

/// One item of the conversation: a message, or a call and its output
/// replayed from an earlier turn.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum TurnItem {
    /// A message: its speaker and the cited text it said.
    Message {
        /// Who spoke the message.
        role: Role,
        /// The cited text of the message.
        text: Ref<Utf8Text>,
    },
    /// A call the model asked for in an earlier turn, replayed as a
    /// `function_call` item.
    Call(ToolCall),
    /// The output of an earlier call, replayed as a `function_call_output`
    /// item.
    CallOutput {
        /// The id of the earlier call this is the output of.
        call_id: CallId,
        /// The program's result, or the reason its arguments did not decode.
        output: ToolOutput,
    },
}

impl TurnItem {
    /// One message citing `text` as spoken by `role`.
    #[must_use]
    pub const fn message(role: Role, text: Ref<Utf8Text>) -> Self {
        Self::Message { role, text }
    }
}

/// What a replayed call produced.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum ToolOutput {
    /// The program's stored result, sent rendered to JSON with its schema.
    Result {
        /// The cited schema of the program's result. The output cites its
        /// own, since a later turn may no longer offer the program.
        schema: Ref<ToolSchema>,
        /// The cited result, stored under the schema's kind.
        result: ErasedRef,
    },
    /// Why the call's arguments did not decode, sent as its stored text.
    Refused(Ref<Utf8Text>),
}

impl ToolOutput {
    /// The result stored at `digest`, cited under the kind of `tool_schema`,
    /// the value `schema` cites.
    #[must_use]
    pub fn result(schema: Ref<ToolSchema>, tool_schema: &ToolSchema, digest: Digest) -> Self {
        Self::Result { schema, result: ErasedRef::new(tool_schema.kind_id(), digest) }
    }
}

/// Why [`TurnItems::new`] or decode refused a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnItemsError {
    /// The conversation had no items.
    Empty,
    /// More than [`TurnItems::MAX_ITEMS`] items.
    TooMany,
    /// The last item was neither a [`Role::User`] message nor a call output.
    LastNotUser,
    /// A call output named no earlier call.
    OrphanOutput,
    /// Two calls shared one call id.
    DuplicateCall,
}

impl TurnItemsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooMany => "too-many",
            Self::LastNotUser => "last-not-user",
            Self::OrphanOutput => "orphan-output",
            Self::DuplicateCall => "duplicate-call",
        }
    }
}

/// The whole conversation, in order: never empty, ending on a user message
/// or a call output, with every call output after the call it answers and no
/// call id twice.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct TurnItems(Vec<TurnItem>);

impl TurnItems {
    /// Most items one turn may carry.
    pub const MAX_ITEMS: usize = 4096;

    /// Accept a conversation that keeps every rule on [`TurnItems`].
    ///
    /// # Errors
    ///
    /// The [`TurnItemsError`] naming the rule the list broke.
    pub fn new(items: Vec<TurnItem>) -> Result<Self, TurnItemsError> {
        Self::check(&items)?;
        Ok(Self(items))
    }

    /// Every item in conversation order.
    #[must_use]
    pub fn as_slice(&self) -> &[TurnItem] {
        &self.0
    }

    fn check(items: &[TurnItem]) -> Result<(), TurnItemsError> {
        let Some(last) = items.last() else {
            return Err(TurnItemsError::Empty);
        };
        if items.len() > Self::MAX_ITEMS {
            return Err(TurnItemsError::TooMany);
        }
        if !matches!(last, TurnItem::Message { role: Role::User, .. } | TurnItem::CallOutput { .. }) {
            return Err(TurnItemsError::LastNotUser);
        }
        let mut calls = BTreeSet::new();
        for item in items {
            match item {
                TurnItem::Message { .. } => {}
                TurnItem::Call(call) => {
                    if !calls.insert(call.call_id()) {
                        return Err(TurnItemsError::DuplicateCall);
                    }
                }
                TurnItem::CallOutput { call_id, .. } => {
                    if !calls.contains(call_id) {
                        return Err(TurnItemsError::OrphanOutput);
                    }
                }
            }
        }
        Ok(())
    }
}

invariant_errors!(TurnItemsError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref, Utf8Text};
    use aether_data::{Storage, StorageData};

    use super::{Role, ToolOutput, TurnItem, TurnItems};
    use crate::input::tests::offered_tool;
    use crate::input::tools::{CallId, OfferedTools};
    use crate::input::{Endpoint, ModelName, OutputBudget, ReasoningEffort, TurnInput};

    #[test]
    fn a_stored_input_that_breaks_a_rule_refuses_on_decode() {
        // Catches a dropped `#[storage(validate)]`, which would let an invalid input in through the journal.
        let input = |tools: OfferedTools, items: TurnItems| TurnInput {
            endpoint: Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
            model: ModelName::new("muse-spark-1.3").expect("model"),
            tools,
            items,
            max_output_tokens: OutputBudget::new(64).expect("budget"),
            reasoning: ReasoningEffort::Low,
        };
        let stored = |input: TurnInput| TurnInput::encode_storage(&StorageData::from_value(input)).expect("encode");
        let decoded = |bytes: &[u8]| TurnInput::decode_storage(bytes).map(|data| data.value);

        let user = TurnItem::message(Role::User, Ref::<Utf8Text>::of_text("hello"));
        let tool = offered_tool(ProgramName::new("muse.turn").expect("program"));
        let tools = OfferedTools::new(vec![tool]).expect("one tool");
        let valid = input(tools, TurnItems::new(vec![user.clone()]).expect("one user item"));
        assert_eq!(decoded(&stored(valid.clone())).ok(), Some(valid), "a valid input decodes");

        let no_tools = OfferedTools::default;
        assert!(decoded(&stored(input(no_tools(), TurnItems(Vec::new())))).is_err(), "an empty list refuses");
        let assistant_last = TurnItems(vec![user, TurnItem::message(Role::Assistant, Ref::of_text("hi"))]);
        assert!(decoded(&stored(input(no_tools(), assistant_last))).is_err(), "an assistant-last list refuses");
        let orphan = TurnItems(vec![TurnItem::CallOutput {
            call_id: CallId::new("call_1").expect("call id"),
            output: ToolOutput::Refused(Ref::of_text("done")),
        }]);
        assert!(decoded(&stored(input(no_tools(), orphan))).is_err(), "an orphan call output refuses");
    }
}
