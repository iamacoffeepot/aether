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
    /// A reasoning item an earlier reply carried, resent ahead of the items
    /// the reply produced.
    Reasoning(Reasoning),
}

impl TurnItem {
    /// One message citing `text` as spoken by `role`.
    #[must_use]
    pub const fn message(role: Role, text: Ref<Utf8Text>) -> Self {
        Self::Message { role, text }
    }
}

/// Why [`ReasoningId::new`] or decode refused a reasoning id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningIdError {
    /// The id was empty.
    Empty,
    /// Longer than [`ReasoningId::MAX_BYTES`].
    TooLong,
    /// A byte was not an ASCII graphic character.
    BadChar,
}

impl ReasoningIdError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::BadChar => "bad-char",
        }
    }
}

/// The vendor's id for one reasoning item: 1 to 256 bytes of ASCII graphic
/// characters.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ReasoningId(String);

impl ReasoningId {
    /// Longest accepted id in bytes.
    pub const MAX_BYTES: usize = 256;

    /// Accept a reasoning id.
    ///
    /// # Errors
    ///
    /// The [`ReasoningIdError`] naming the rule the id broke.
    pub fn new(id: impl Into<String>) -> Result<Self, ReasoningIdError> {
        let id = id.into();
        Self::check(&id)?;
        Ok(Self(id))
    }

    /// Borrow the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(id: &str) -> Result<(), ReasoningIdError> {
        if id.is_empty() {
            return Err(ReasoningIdError::Empty);
        }
        if id.len() > Self::MAX_BYTES {
            return Err(ReasoningIdError::TooLong);
        }
        let graphic = id.bytes().all(|byte| byte.is_ascii_graphic());
        if !graphic {
            return Err(ReasoningIdError::BadChar);
        }
        Ok(())
    }
}

/// A reply's reasoning item, kept so the next turn resends it: the vendor's
/// id and the cited encrypted content, which the client hands back as sent.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct Reasoning {
    /// The vendor's id for the item, resent as sent.
    id: ReasoningId,
    /// The cited encrypted content, resent as sent.
    encrypted: Ref<Utf8Text>,
}

impl Reasoning {
    /// A reasoning item with `id`, citing its encrypted content.
    #[must_use]
    pub const fn new(id: ReasoningId, encrypted: Ref<Utf8Text>) -> Self {
        Self { id, encrypted }
    }

    /// The vendor's id for the item.
    #[must_use]
    pub const fn id(&self) -> &ReasoningId {
        &self.id
    }

    /// The cited encrypted content.
    #[must_use]
    pub const fn encrypted(&self) -> Ref<Utf8Text> {
        self.encrypted
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

    /// A conversation of the instructions as the leading developer message and
    /// one user message citing `user`, which keeps every rule.
    pub(crate) fn opening(instructions: Ref<Utf8Text>, user: Ref<Utf8Text>) -> Self {
        Self(vec![TurnItem::message(Role::Developer, instructions), TurnItem::message(Role::User, user)])
    }

    /// Every item in conversation order.
    #[must_use]
    pub fn as_slice(&self) -> &[TurnItem] {
        &self.0
    }

    fn check(items: &[TurnItem]) -> Result<(), TurnItemsError> {
        check_order(items)?;
        if !matches!(items.last(), Some(TurnItem::Message { role: Role::User, .. } | TurnItem::CallOutput { .. })) {
            return Err(TurnItemsError::LastNotUser);
        }
        Ok(())
    }
}

/// The rules every conversation keeps wherever it ends: never empty, at most
/// [`TurnItems::MAX_ITEMS`] items, no call id twice, and every call output
/// after the call it answers. Answers the ids of the calls no output answers.
pub fn check_order(items: &[TurnItem]) -> Result<BTreeSet<&CallId>, TurnItemsError> {
    if items.is_empty() {
        return Err(TurnItemsError::Empty);
    }
    if items.len() > TurnItems::MAX_ITEMS {
        return Err(TurnItemsError::TooMany);
    }
    let mut calls = BTreeSet::new();
    let mut unanswered = BTreeSet::new();
    for item in items {
        match item {
            TurnItem::Message { .. } | TurnItem::Reasoning(_) => {}
            TurnItem::Call(call) => {
                if !calls.insert(call.call_id()) {
                    return Err(TurnItemsError::DuplicateCall);
                }
                unanswered.insert(call.call_id());
            }
            TurnItem::CallOutput { call_id, .. } => {
                if !calls.contains(call_id) {
                    return Err(TurnItemsError::OrphanOutput);
                }
                unanswered.remove(call_id);
            }
        }
    }
    Ok(unanswered)
}

invariant_errors!(TurnItemsError, ReasoningIdError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref, Utf8Text};
    use aether_data::{Storage, StorageData};

    use super::{Role, ToolOutput, TurnItem, TurnItems};
    use crate::input::tests::offered_tool;
    use crate::input::tools::{CallId, OfferedTools};
    use crate::input::{Endpoint, InputLimit, ModelName, OutputBudget, ReasoningEffort, TurnInput};

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
            input_limit: InputLimit::new(u64::MAX).expect("limit"),
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
