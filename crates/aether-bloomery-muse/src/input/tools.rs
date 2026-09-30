//! The programs a turn offers as tools, and the calls a model asks for.
//!
//! A tool is a program to run. The caller names every offered program and
//! cites the definition it rendered with
//! `aether_bloomery_program::tool_definition`, so the recorded input holds
//! exactly what was sent; nothing defaults to every declared program. The
//! caller also cites the program's input and result schemas
//! ([`ToolSchema::of`]), so `muse.turn` links no tool's types.
//!
//! A call cites its arguments exactly as the model wrote them, to replay
//! them unchanged, and cites what `muse.turn` made of them against the
//! offered input schema: the decoded input, stored under the input's kind,
//! or the text of the refused decode.

use std::collections::BTreeSet;

use aether_bloomery_kinds::{ErasedRef, ProgramName, Ref, Utf8Text};
use aether_bloomery_program::ToolSchema;

/// One program offered to the model, with the definition sent for it and
/// the schemas of its input and result.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct OfferedTool {
    /// The program the model may ask to run.
    program: ProgramName,
    /// The cited responses-API function definition sent for the program: a
    /// JSON object whose `name` is the program's function name.
    definition: Ref<Utf8Text>,
    /// The cited schema of the program's input, which a call's arguments
    /// decode against.
    input: Ref<ToolSchema>,
    /// The cited schema of the program's result, which the call's output
    /// renders with.
    result: Ref<ToolSchema>,
}

impl OfferedTool {
    /// Offer `program`, sending the cited `definition` for it, with the cited
    /// schemas of its `input` and `result`.
    #[must_use]
    pub const fn new(
        program: ProgramName,
        definition: Ref<Utf8Text>,
        input: Ref<ToolSchema>,
        result: Ref<ToolSchema>,
    ) -> Self {
        Self { program, definition, input, result }
    }

    /// The program the model may ask to run.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// The cited definition sent for the program.
    #[must_use]
    pub const fn definition(&self) -> Ref<Utf8Text> {
        self.definition
    }

    /// The cited schema of the program's input.
    #[must_use]
    pub const fn input(&self) -> Ref<ToolSchema> {
        self.input
    }

    /// The cited schema of the program's result.
    #[must_use]
    pub const fn result(&self) -> Ref<ToolSchema> {
        self.result
    }
}

/// Why [`OfferedTools::new`] or decode refused a tool list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferedToolsError {
    /// More than [`OfferedTools::MAX_TOOLS`] tools.
    TooMany,
    /// One program was offered twice.
    DuplicateProgram,
}

impl OfferedToolsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::TooMany => "too-many",
            Self::DuplicateProgram => "duplicate-program",
        }
    }
}

/// The programs a turn offers: at most 128, no program twice. An empty list
/// offers none.
#[derive(Debug, Clone, PartialEq, Eq, Default, aether_data::Storage)]
#[storage(validate)]
pub struct OfferedTools(Vec<OfferedTool>);

impl OfferedTools {
    /// Most tools one turn may offer.
    pub const MAX_TOOLS: usize = 128;

    /// Accept a tool list that names each program once.
    ///
    /// # Errors
    ///
    /// The [`OfferedToolsError`] naming the rule the list broke.
    pub fn new(tools: Vec<OfferedTool>) -> Result<Self, OfferedToolsError> {
        Self::check(&tools)?;
        Ok(Self(tools))
    }

    /// Every offered tool, in the order sent.
    #[must_use]
    pub fn as_slice(&self) -> &[OfferedTool] {
        &self.0
    }

    fn check(tools: &[OfferedTool]) -> Result<(), OfferedToolsError> {
        if tools.len() > Self::MAX_TOOLS {
            return Err(OfferedToolsError::TooMany);
        }
        let mut programs = BTreeSet::new();
        if !tools.iter().all(|tool| programs.insert(&tool.program)) {
            return Err(OfferedToolsError::DuplicateProgram);
        }
        Ok(())
    }
}

/// Why [`CallId::new`] or decode refused a call id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallIdError {
    /// The id was empty.
    Empty,
    /// Longer than [`CallId::MAX_BYTES`].
    TooLong,
    /// A byte was not an ASCII graphic character.
    BadChar,
}

impl CallIdError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::BadChar => "bad-char",
        }
    }
}

/// The vendor's id for one call, which pairs the call with its output:
/// 1 to 256 bytes of ASCII graphic characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, aether_data::Storage)]
#[storage(validate)]
pub struct CallId(String);

impl CallId {
    /// Longest accepted id in bytes.
    pub const MAX_BYTES: usize = 256;

    /// Accept a call id.
    ///
    /// # Errors
    ///
    /// The [`CallIdError`] naming the rule the id broke.
    pub fn new(id: impl Into<String>) -> Result<Self, CallIdError> {
        let id = id.into();
        Self::check(&id)?;
        Ok(Self(id))
    }

    /// Borrow the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(id: &str) -> Result<(), CallIdError> {
        if id.is_empty() {
            return Err(CallIdError::Empty);
        }
        if id.len() > Self::MAX_BYTES {
            return Err(CallIdError::TooLong);
        }
        if !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CallIdError::BadChar);
        }
        Ok(())
    }
}

/// What a call's arguments decoded to against the offered input schema.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum ToolInput {
    /// The program's input, stored under the input's kind: the input to run
    /// the program with.
    Decoded(ErasedRef),
    /// Why the arguments did not decode: the text to replay as the call's
    /// output.
    Refused(Ref<Utf8Text>),
}

/// One call the model asked for: its id, the program, the arguments as the
/// model wrote them, and the input they decoded to.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct ToolCall {
    /// The vendor's id for the call, which its output names.
    call_id: CallId,
    /// The program the model asked to run.
    program: ProgramName,
    /// The cited arguments, verbatim: JSON the program's input may or may
    /// not decode from.
    arguments: Ref<Utf8Text>,
    /// The decoded input, or the refused decode's text.
    input: ToolInput,
}

impl ToolCall {
    /// One call `call_id` to `program` with the cited `arguments`, which
    /// decoded to `input`.
    #[must_use]
    pub const fn new(call_id: CallId, program: ProgramName, arguments: Ref<Utf8Text>, input: ToolInput) -> Self {
        Self { call_id, program, arguments, input }
    }

    /// The vendor's id for the call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The program the model asked to run.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// The cited arguments, verbatim.
    #[must_use]
    pub const fn arguments(&self) -> Ref<Utf8Text> {
        self.arguments
    }

    /// The decoded input, or the refused decode's text.
    #[must_use]
    pub const fn input(&self) -> &ToolInput {
        &self.input
    }
}

/// Why [`ToolCalls::new`] or decode refused a call list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallsError {
    /// The list had no calls.
    Empty,
    /// More than [`ToolCalls::MAX_CALLS`] calls.
    TooMany,
    /// Two calls shared one call id.
    DuplicateCall,
}

impl ToolCallsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooMany => "too-many",
            Self::DuplicateCall => "duplicate-call",
        }
    }
}

/// Every call one reply asked for, in order: 1 to 128, no call id twice.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct ToolCalls(Vec<ToolCall>);

impl ToolCalls {
    /// Most calls one reply may ask for.
    pub const MAX_CALLS: usize = 128;

    /// Accept a non-empty call list whose ids are distinct.
    ///
    /// # Errors
    ///
    /// The [`ToolCallsError`] naming the rule the list broke.
    pub fn new(calls: Vec<ToolCall>) -> Result<Self, ToolCallsError> {
        Self::check(&calls)?;
        Ok(Self(calls))
    }

    /// Every call, in the order the reply asked for them.
    #[must_use]
    pub fn as_slice(&self) -> &[ToolCall] {
        &self.0
    }

    fn check(calls: &[ToolCall]) -> Result<(), ToolCallsError> {
        Self::check_ids(calls.iter().map(ToolCall::call_id))
    }

    /// The list rules over the call ids alone, so a reply is checked before
    /// any of its arguments is staged.
    pub(crate) fn check_ids<'a>(mut ids: impl ExactSizeIterator<Item = &'a CallId>) -> Result<(), ToolCallsError> {
        match ids.len() {
            0 => return Err(ToolCallsError::Empty),
            len if len > Self::MAX_CALLS => return Err(ToolCallsError::TooMany),
            _ => {}
        }
        let mut seen = BTreeSet::new();
        if !ids.all(|id| seen.insert(id)) {
            return Err(ToolCallsError::DuplicateCall);
        }
        Ok(())
    }
}

invariant_errors!(OfferedToolsError, CallIdError, ToolCallsError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref};
    use aether_data::{Storage, StorageData};

    use super::{CallId, CallIdError, OfferedTools, OfferedToolsError, ToolCalls, ToolCallsError};
    use crate::input::tests::{call, offered_tool};
    use crate::input::{Endpoint, ModelName, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems};

    #[test]
    fn each_tool_rule_refuses_and_accepts_its_neighbour() {
        // Catches a rule that refuses a valid neighbour or admits its violation.
        let longest_id = "a".repeat(CallId::MAX_BYTES);
        let too_long_id = format!("{longest_id}a");
        let ids = [
            ("", CallIdError::Empty, "a"),
            (too_long_id.as_str(), CallIdError::TooLong, longest_id.as_str()),
            ("call 1", CallIdError::BadChar, "call_1"),
            ("call\u{e9}", CallIdError::BadChar, "call-e"),
        ];
        for (reject, error, accept) in ids {
            assert_eq!(CallId::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(CallId::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }

        let program = |name: &str| ProgramName::new(name).expect("program name");
        let tool = |name: &str| offered_tool(program(name));
        let distinct = |count: usize| (0..count).map(|index| tool(&format!("tool.t{index}"))).collect::<Vec<_>>();
        let tools = [
            (vec![tool("muse.turn"), tool("muse.turn")], OfferedToolsError::DuplicateProgram, distinct(2)),
            (distinct(OfferedTools::MAX_TOOLS + 1), OfferedToolsError::TooMany, distinct(OfferedTools::MAX_TOOLS)),
        ];
        for (reject, error, accept) in tools {
            assert_eq!(OfferedTools::new(reject), Err(error));
            assert_eq!(OfferedTools::new(accept.clone()).expect("accepted neighbour").as_slice(), accept.as_slice());
        }

        let call = |id: &str| call(id, "muse.turn");
        let calls = [
            (Vec::new(), ToolCallsError::Empty, vec![call("a")]),
            (vec![call("a"), call("a")], ToolCallsError::DuplicateCall, vec![call("a"), call("b")]),
        ];
        for (reject, error, accept) in calls {
            assert_eq!(ToolCalls::new(reject), Err(error));
            assert_eq!(ToolCalls::new(accept.clone()).expect("accepted neighbour").as_slice(), accept.as_slice());
        }
    }

    #[test]
    fn a_stored_input_offering_a_program_twice_refuses_on_decode() {
        // Catches a dropped `#[storage(validate)]` on the tool list, which would let a doubled offer in through
        // the journal.
        let tool = offered_tool(ProgramName::new("muse.turn").expect("program"));
        let input = TurnInput {
            endpoint: Endpoint::new("https://example.test/v1/responses").expect("endpoint"),
            model: ModelName::new("muse-spark-1.3").expect("model"),
            tools: OfferedTools(vec![tool.clone(), tool]),
            items: TurnItems::new(vec![TurnItem::message(Role::User, Ref::of_text("hello"))]).expect("items"),
            max_output_tokens: OutputBudget::new(64).expect("budget"),
            reasoning: ReasoningEffort::Low,
        };

        let stored = TurnInput::encode_storage(&StorageData::from_value(input)).expect("encode");
        assert!(TurnInput::decode_storage(&stored).is_err());
    }
}
