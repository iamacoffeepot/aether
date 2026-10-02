//! The programs a turn offers as tools, and the calls a model asks for.
//!
//! A tool is a program to run. The caller names every offered program, the
//! head of the bundle it lives in, which the loop calls it through, and cites
//! the definition it rendered with
//! `aether_bloomery_program::tool_definition`, so the recorded input holds
//! exactly what was sent; nothing defaults to every declared program. The
//! caller also cites the program's input and result schemas
//! ([`ToolSchema::of`]), so `muse.turn` links no tool's types. The offer also
//! cites the bound value the loop binds into every call, which the model
//! never sees; its kind is the one the tool decodes it as.
//!
//! A call cites its arguments exactly as the model wrote them, to replay
//! them unchanged, and records what `muse.turn` made of the call: a call to
//! an offered program whose arguments decoded carries the program and the
//! decoded input, stored under the input's kind; any other call carries the
//! name the model wrote and the text that refuses it.

use std::borrow::Cow;
use std::collections::BTreeSet;

use aether_bloomery_kinds::{ErasedRef, Head, OpaqueBytes, ProgramName, Ref, Utf8Text};
use aether_bloomery_program::{ToolDefinitionError, ToolSchema, function_name};

/// One program offered to the model, with the head of the bundle it lives in,
/// the definition sent for it, the schemas of its input and result, and the
/// bound value the loop binds.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct OfferedTool {
    /// The program the model may ask to run.
    program: ProgramName,
    /// The head of the bundle the program lives in, which the loop calls it
    /// through. Never sent to the model.
    head: Head<OpaqueBytes>,
    /// The cited responses-API function definition sent for the program: a
    /// JSON object whose `name` is the program's function name.
    definition: Ref<Utf8Text>,
    /// The cited schema of the program's input, which a call's arguments
    /// decode against.
    input: Ref<ToolSchema>,
    /// The value the loop binds into every call's input, unseen by the
    /// model; its kind is the bound kind the program takes.
    bound: ErasedRef,
    /// The cited schema of the program's result, which the call's output
    /// renders with.
    result: Ref<ToolSchema>,
}

impl OfferedTool {
    /// Offer `program` from the bundle `head` resolves to, sending the cited
    /// `definition` for it, with the cited schemas of its `input` and
    /// `result`, binding `bound` into every call.
    #[must_use]
    pub const fn new(
        program: ProgramName,
        head: Head<OpaqueBytes>,
        definition: Ref<Utf8Text>,
        input: Ref<ToolSchema>,
        bound: ErasedRef,
        result: Ref<ToolSchema>,
    ) -> Self {
        Self { program, head, definition, input, bound, result }
    }

    /// Whether `other` offers the same tool: the same program from the same
    /// bundle head, definition, and schemas, and a bound of the same kind,
    /// whatever its value.
    #[must_use]
    pub fn same_tool(&self, other: &Self) -> bool {
        self.program == other.program
            && self.head == other.head
            && self.definition == other.definition
            && self.input == other.input
            && self.result == other.result
            && self.bound.kind() == other.bound.kind()
    }

    /// The program the model may ask to run.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// The head of the bundle the program lives in.
    #[must_use]
    pub const fn head(&self) -> &Head<OpaqueBytes> {
        &self.head
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

    /// The bound value the loop binds for the program.
    #[must_use]
    pub const fn bound(&self) -> ErasedRef {
        self.bound
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

    /// Whether `tool` is one of these offers, whatever bound value it
    /// carries.
    #[must_use]
    pub fn offers(&self, tool: &OfferedTool) -> bool {
        self.0.iter().any(|offer| offer.same_tool(tool))
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

/// Why [`FunctionName::new`] or decode refused a function name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionNameError {
    /// The name was empty.
    Empty,
    /// Longer than [`FunctionName::MAX_BYTES`].
    TooLong,
}

impl FunctionNameError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
        }
    }
}

/// The function name a model wrote on a call, kept exactly as written so the
/// call replays under it: 1 to 256 bytes of any UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct FunctionName(String);

impl FunctionName {
    /// Longest accepted name in bytes.
    pub const MAX_BYTES: usize = 256;

    /// Accept a function name.
    ///
    /// # Errors
    ///
    /// The [`FunctionNameError`] naming the rule the name broke.
    pub fn new(name: impl Into<String>) -> Result<Self, FunctionNameError> {
        let name = name.into();
        Self::check(&name)?;
        Ok(Self(name))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(name: &str) -> Result<(), FunctionNameError> {
        if name.is_empty() {
            return Err(FunctionNameError::Empty);
        }
        if name.len() > Self::MAX_BYTES {
            return Err(FunctionNameError::TooLong);
        }
        Ok(())
    }
}

/// What `muse.turn` made of a call: the input to run an offered program
/// with, or the text that refuses the call.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum ToolInput {
    /// A call to an offered program whose arguments decoded; it replays under
    /// the program's function name, which is the name the model wrote.
    Decoded {
        /// The program to run.
        program: ProgramName,
        /// The program's input, stored under the input's kind.
        input: ErasedRef,
    },
    /// A call that does not run: its name offers no program, or its
    /// arguments did not decode.
    Refused {
        /// The function name exactly as the model wrote it, which the call
        /// replays under.
        name: FunctionName,
        /// Why the call does not run: the text to replay as its output.
        refusal: Ref<Utf8Text>,
    },
}

/// One call the model asked for: its id, the arguments as the model wrote
/// them, and what `muse.turn` made of the call.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct ToolCall {
    /// The vendor's id for the call, which its output names.
    call_id: CallId,
    /// The cited arguments, verbatim: JSON the program's input may or may
    /// not decode from.
    arguments: Ref<Utf8Text>,
    /// The program and its decoded input, or the call's name and refusal.
    input: ToolInput,
}

impl ToolCall {
    /// One call `call_id` to the offered `program`, whose cited `arguments`
    /// decoded to `input`.
    #[must_use]
    pub const fn decoded(call_id: CallId, program: ProgramName, arguments: Ref<Utf8Text>, input: ErasedRef) -> Self {
        Self { call_id, arguments, input: ToolInput::Decoded { program, input } }
    }

    /// One call `call_id` named `name` with the cited `arguments`, which does
    /// not run and is answered with `refusal`.
    #[must_use]
    pub const fn refused(
        call_id: CallId,
        name: FunctionName,
        arguments: Ref<Utf8Text>,
        refusal: Ref<Utf8Text>,
    ) -> Self {
        Self { call_id, arguments, input: ToolInput::Refused { name, refusal } }
    }

    /// The vendor's id for the call.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The program the call runs, or `None` for a refused call.
    #[must_use]
    pub const fn program(&self) -> Option<&ProgramName> {
        match &self.input {
            ToolInput::Decoded { program, .. } => Some(program),
            ToolInput::Refused { .. } => None,
        }
    }

    /// The function name the call replays under: the program's function name
    /// for a decoded call, the name as written for a refused one.
    ///
    /// # Errors
    ///
    /// The [`ToolDefinitionError`] of a program whose function name is too
    /// long.
    pub fn name(&self) -> Result<Cow<'_, str>, ToolDefinitionError> {
        match &self.input {
            ToolInput::Decoded { program, .. } => function_name(program).map(Cow::Owned),
            ToolInput::Refused { name, .. } => Ok(Cow::Borrowed(name.as_str())),
        }
    }

    /// The cited arguments, verbatim.
    #[must_use]
    pub const fn arguments(&self) -> Ref<Utf8Text> {
        self.arguments
    }

    /// The program and its decoded input, or the call's name and refusal.
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

invariant_errors!(OfferedToolsError, CallIdError, FunctionNameError, ToolCallsError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref};
    use aether_data::{Storage, StorageData};

    use super::{
        CallId, CallIdError, FunctionName, FunctionNameError, OfferedTools, OfferedToolsError, ToolCalls,
        ToolCallsError,
    };
    use crate::input::tests::{call, offered_tool};
    use crate::input::{
        Endpoint, InputLimit, ModelName, OutputBudget, ReasoningEffort, Role, TurnInput, TurnItem, TurnItems,
    };

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

        let longest_name = "\u{e9}".repeat(FunctionName::MAX_BYTES / 2);
        let too_long_name = format!("{longest_name}a");
        let names = [
            ("", FunctionNameError::Empty, "a"),
            (too_long_name.as_str(), FunctionNameError::TooLong, longest_name.as_str()),
        ];
        for (reject, error, accept) in names {
            assert_eq!(FunctionName::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(FunctionName::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
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
            input_limit: InputLimit::new(u64::MAX).expect("limit"),
        };

        let stored = TurnInput::encode_storage(&StorageData::from_value(input)).expect("encode");
        assert!(TurnInput::decode_storage(&stored).is_err());
    }
}
