//! `muse.session`: everything a session needs to be picked up again, and the
//! values that name and budget one.

use core::borrow::Borrow;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{Detail, FaultReason, Head, ProgramName, Ref, Tree, Utf8Text};
use aether_data::Invariant;

use crate::input::{
    Endpoint, InputLimit, ModelName, OfferedTool, OfferedTools, OutputBudget, ReasoningEffort, Role, TurnInput,
    TurnItem, TurnItems, TurnItemsError, check_order,
};
use crate::result::TurnResult;

/// Every field of a turn but its conversation: what a session sends with each
/// turn.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct TurnSettings {
    /// The absolute `https://` or `http://` URL of the responses endpoint each
    /// turn posts to.
    endpoint: Endpoint,
    /// The model that answers: 1 to 128 bytes matching `[a-z0-9][a-z0-9._-]*`.
    model: ModelName,
    /// The programs offered to the model as tools, each with the definition
    /// sent for it: at most 128, no program twice. Empty offers none.
    tools: OfferedTools,
    /// The most output tokens, reasoning included, one turn may produce.
    /// Never zero.
    max_output_tokens: OutputBudget,
    /// How much the model reasons before it answers.
    reasoning: ReasoningEffort,
    /// The most input tokens one turn may have been billed for before the
    /// session rests with [`RestReason::ContextFull`]. Never zero.
    input_limit: InputLimit,
}

impl TurnSettings {
    /// The one constructor. Each part already holds its own rules.
    #[must_use]
    pub const fn new(
        endpoint: Endpoint,
        model: ModelName,
        tools: OfferedTools,
        max_output_tokens: OutputBudget,
        reasoning: ReasoningEffort,
        input_limit: InputLimit,
    ) -> Self {
        Self { endpoint, model, tools, max_output_tokens, reasoning, input_limit }
    }

    /// The programs offered as tools, in the order sent.
    #[must_use]
    pub fn tools(&self) -> &[OfferedTool] {
        self.tools.as_slice()
    }

    /// The most input tokens one turn may have been billed for before the
    /// session rests.
    #[must_use]
    pub const fn input_limit(&self) -> InputLimit {
        self.input_limit
    }

    /// The first turn of a session: these settings with the instructions as
    /// the leading developer message and one user message citing `user`.
    #[must_use]
    pub fn open(&self, instructions: Ref<Utf8Text>, user: Ref<Utf8Text>) -> TurnInput {
        self.clone().with_items(TurnItems::opening(instructions, user))
    }

    /// These settings with `max_output_tokens` as the budget.
    pub(crate) fn with_budget(mut self, max_output_tokens: OutputBudget) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    /// One turn sending `items` with these settings.
    pub(crate) fn with_items(self, items: TurnItems) -> TurnInput {
        TurnInput::new(
            self.endpoint,
            self.model,
            self.tools,
            items,
            self.max_output_tokens,
            self.reasoning,
            self.input_limit,
        )
    }
}

/// Why [`SessionItems::new`] or decode refused a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionItemsError {
    /// The items broke a rule every conversation keeps.
    Items(TurnItemsError),
    /// A call had no output answering it.
    Unanswered,
}

impl Invariant for SessionItemsError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Items(error) => Invariant::reason(error),
            Self::Unanswered => "unanswered-call",
        }
    }
}

impl fmt::Display for SessionItemsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Invariant::reason(self))
    }
}

impl Error for SessionItemsError {}

impl From<TurnItemsError> for SessionItemsError {
    fn from(error: TurnItemsError) -> Self {
        Self::Items(error)
    }
}

/// A session's whole conversation at rest, in order: the rules of
/// [`TurnItems`] except where it ends, since a session may end on the
/// assistant's reply, and every call answered by an output, since a session at
/// rest owes none.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct SessionItems(Vec<TurnItem>);

impl SessionItems {
    /// Accept a conversation that keeps every rule on [`SessionItems`].
    ///
    /// # Errors
    ///
    /// The [`SessionItemsError`] naming the rule the list broke.
    pub fn new(items: Vec<TurnItem>) -> Result<Self, SessionItemsError> {
        Self::check(&items)?;
        Ok(Self(items))
    }

    /// Every item in conversation order.
    #[must_use]
    pub fn as_slice(&self) -> &[TurnItem] {
        &self.0
    }

    fn check(items: &[TurnItem]) -> Result<(), SessionItemsError> {
        if check_order(items)?.is_empty() {
            Ok(())
        } else {
            Err(SessionItemsError::Unanswered)
        }
    }
}

/// Why [`TurnLimit::new`] or decode refused a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLimitError {
    /// The limit was zero.
    Zero,
}

impl TurnLimitError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Zero => "zero",
        }
    }
}

/// The most `muse.turn` runs one open or continue may make before the session
/// is made to rest. It counts turns, never tool calls, and is never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct TurnLimit(u32);

impl TurnLimit {
    /// Accept a limit of at least one turn.
    ///
    /// # Errors
    ///
    /// [`TurnLimitError::Zero`] for zero.
    pub fn new(turns: u32) -> Result<Self, TurnLimitError> {
        Self::check(turns)?;
        Ok(Self(turns))
    }

    /// The most turns.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    // `#[storage(validate)]` calls `check(&inner)`; `Borrow` takes that
    // reference and `new`'s owned value alike.
    fn check(turns: impl Borrow<u32>) -> Result<(), TurnLimitError> {
        if *turns.borrow() == 0 {
            Err(TurnLimitError::Zero)
        } else {
            Ok(())
        }
    }
}

/// Why a session rests.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum RestReason {
    /// The model finished its answer.
    Completed,
    /// The model refused.
    Declined,
    /// The model stopped early, for example on the output budget.
    Incomplete,
    /// The activation made as many turns as its limit allowed, and every call
    /// the last one asked for has its output.
    TurnLimit,
    /// A turn that would go on reached the session's input limit, and every
    /// call it asked for has its output.
    ContextFull,
    /// The activation stopped without a turn resting it.
    Failed(Failure),
}

/// Why a session stopped without a turn resting it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum Failure {
    /// A run the loop requested faulted: a tool, `muse.turn`, the clock's
    /// wait, or `muse.session.record`.
    Faulted {
        /// The program whose run faulted.
        program: ProgramName,
        /// Why the driver recorded the fault.
        reason: FaultReason,
    },
    /// One of the loop's own rules produced no records: the driver's
    /// `ReactionFailed` reason.
    Reaction {
        /// The reason the driver recorded.
        reason: Detail,
    },
    /// The vendor ended the turn: `Rejected`, `Unreadable`, or `Transient`
    /// past the retry cap.
    Turn {
        /// The cited result of the turn that ended the session.
        result: Ref<TurnResult>,
    },
    /// The loop could not build its next request from its own records: the
    /// next turn would pass [`TurnItems::MAX_ITEMS`], an input did not
    /// encode, or a run's result answered no offered call.
    Unbuilt {
        /// What the loop could not build.
        reason: Detail,
    },
}

/// The name of one session: the seq of the `muse.session.open` run that
/// opened it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, aether_data::Storage)]
pub struct SessionKey(
    /// The seq of the `muse.session.open` run that opened the session.
    u64,
);

impl SessionKey {
    /// The session opened by the `muse.session.open` run recorded at `seq`.
    #[must_use]
    pub const fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// The seq of the run that opened the session.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The head naming the session's latest [`Session`]: the key in decimal.
    ///
    /// # Panics
    ///
    /// Never: a decimal number keeps every head name rule.
    #[must_use]
    pub fn head(self) -> Head<Session> {
        Head::named(self.0.to_string()).expect("a decimal name keeps every head name rule")
    }

    /// The session a [`Session`] head names, or `None` for a head no key
    /// names.
    pub(crate) fn of_head(head: &Head<Session>) -> Option<Self> {
        let key = Self(head.as_str().parse().ok()?);
        (key.head() == *head).then_some(key)
    }
}

/// A session at rest: its settings, its whole conversation, why it rests, and
/// the tree its tools left.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session")]
pub struct Session {
    /// What every turn of the session sends besides its conversation.
    settings: TurnSettings,
    /// The whole conversation, which may end on the assistant's reply.
    items: SessionItems,
    /// Why the session rests.
    rested: RestReason,
    /// The tree the session's tools left: the latest tree of the session.
    tree: Ref<Tree>,
}

impl Session {
    /// A session at rest.
    pub(crate) const fn new(settings: TurnSettings, items: SessionItems, rested: RestReason, tree: Ref<Tree>) -> Self {
        Self { settings, items, rested, tree }
    }

    /// What every turn sends besides its conversation.
    #[must_use]
    pub const fn settings(&self) -> &TurnSettings {
        &self.settings
    }

    /// The whole conversation, in order.
    #[must_use]
    pub fn items(&self) -> &[TurnItem] {
        self.items.as_slice()
    }

    /// Why the session rests.
    #[must_use]
    pub const fn rested(&self) -> &RestReason {
        &self.rested
    }

    /// The tree the session's tools left, which a continue works on.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// The next turn: the settings, with `max_output_tokens` as the budget
    /// when given, and the conversation, followed by one user message citing
    /// `user` when given. With no message the conversation is resent as it
    /// stands. The one way from a session to its next turn.
    ///
    /// # Errors
    ///
    /// [`TurnItemsError::TooMany`] when the message passes
    /// [`TurnItems::MAX_ITEMS`], and [`TurnItemsError::LastNotUser`] for no
    /// message on a conversation that ends on the assistant's reply.
    pub fn continue_with(
        &self,
        user: Option<Ref<Utf8Text>>,
        max_output_tokens: Option<OutputBudget>,
    ) -> Result<TurnInput, TurnItemsError> {
        let items = self.items().iter().cloned().chain(user.map(|user| TurnItem::message(Role::User, user))).collect();
        let mut settings = self.settings.clone();
        if let Some(budget) = max_output_tokens {
            settings = settings.with_budget(budget);
        }
        Ok(settings.with_items(TurnItems::new(items)?))
    }
}

invariant_errors!(TurnLimitError);

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Ref, Tree};
    use aether_data::{Storage, StorageData};

    use super::{RestReason, Session, SessionItems, SessionItemsError, TurnLimit, TurnLimitError};
    use crate::input::tests::call;
    use crate::input::{CallId, OfferedTools, OutputBudget, Role, ToolOutput, TurnItem, TurnItemsError};
    use crate::session::fixture::settings;
    use crate::session::open::OpenInput;

    fn message(role: Role, text: &str) -> TurnItem {
        TurnItem::message(role, Ref::of_text(text))
    }

    fn output(id: &str) -> TurnItem {
        let call_id = CallId::new(id).expect("call id");
        TurnItem::CallOutput { call_id, output: ToolOutput::Refused(Ref::of_text("refused")) }
    }

    #[test]
    fn a_session_may_end_on_the_reply_but_never_owes_an_output_and_a_limit_is_never_zero() {
        // Catches the turn rule's last-item check applied to a session at rest, a session admitted with a call no
        // output answers, and a zero turn limit, whether built or read back from the journal.
        let answered = vec![message(Role::User, "hi"), message(Role::Assistant, "hello")];
        assert!(SessionItems::new(answered).is_ok(), "a session may end on the assistant's reply");
        assert_eq!(SessionItems::new(Vec::new()), Err(SessionItemsError::Items(TurnItemsError::Empty)));

        let unanswered = vec![message(Role::User, "hi"), TurnItem::Call(call("a", "muse.echo"))];
        assert_eq!(SessionItems::new(unanswered.clone()), Err(SessionItemsError::Unanswered));
        let stored = Session {
            settings: settings(OfferedTools::default()),
            items: SessionItems(unanswered),
            rested: RestReason::TurnLimit,
            tree: Ref::of_encoded(&Tree::empty()).expect("tree"),
        };
        let bytes = Session::encode_storage(&StorageData::from_value(stored)).expect("encode");
        assert!(Session::decode_storage(&bytes).is_err(), "an unanswered call refuses on decode");

        assert_eq!(TurnLimit::new(0), Err(TurnLimitError::Zero));
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let zero = OpenInput::new(
            settings(OfferedTools::default()),
            Ref::of_text("rules"),
            Ref::of_text("hi"),
            TurnLimit(0),
            tree,
            Vec::new(),
        );
        let bytes = OpenInput::encode_storage(&StorageData::from_value(zero)).expect("encode");
        assert!(OpenInput::decode_storage(&bytes).is_err(), "a zero limit refuses on decode");
    }

    #[test]
    fn a_continued_turn_is_the_session_followed_by_the_user_message() {
        // Catches a continue that drops or reorders the session's items or changes a setting, which would break
        // the prompt prefix the next turn resends.
        let items = vec![
            message(Role::User, "hi"),
            TurnItem::Call(call("a", "muse.echo")),
            output("a"),
            message(Role::Assistant, "hello"),
        ];
        let session = Session {
            settings: settings(OfferedTools::default()),
            items: SessionItems::new(items.clone()).expect("items"),
            rested: RestReason::Completed,
            tree: Ref::of_encoded(&Tree::empty()).expect("tree"),
        };

        let next = session.continue_with(Some(Ref::of_text("more")), None).expect("next turn");
        assert_eq!(next.settings(), *session.settings());
        assert_eq!(next.items().split_last(), Some((&message(Role::User, "more"), items.as_slice())));
        assert_eq!(session.continue_with(None, None), Err(TurnItemsError::LastNotUser), "a reply is never resent");
    }

    #[test]
    fn a_resent_turn_is_the_session_as_it_stands_with_only_the_budget_replaced() {
        // Catches a resend that appends anything to the conversation or changes a setting besides the budget, which
        // would bias the model or break the prompt prefix the failed turn sent.
        let items = vec![message(Role::User, "hi"), TurnItem::Call(call("a", "muse.echo")), output("a")];
        let session = Session {
            settings: settings(OfferedTools::default()),
            items: SessionItems::new(items.clone()).expect("items"),
            rested: RestReason::Incomplete,
            tree: Ref::of_encoded(&Tree::empty()).expect("tree"),
        };
        let budget = OutputBudget::new(4096).expect("budget");

        let next = session.continue_with(None, Some(budget)).expect("a resent turn");
        assert_eq!(next.items(), items.as_slice());
        assert_eq!(next.max_output_tokens(), budget);
        assert_eq!(next.settings(), session.settings().clone().with_budget(budget));
    }
}
