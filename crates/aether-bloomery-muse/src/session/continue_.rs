//! `muse.session.continue`: the one way a rested session takes its next turn.

use aether_bloomery_kinds::{Detail, Mode, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_data::{Ref, Utf8Text};

use crate::input::{OutputBudget, TurnInput};
use crate::session::state::{Session, SessionKey, TurnLimit};

/// A rested session to continue: which session, the [`Session`] it continues
/// from, the next user message if any, the output budget if it changes, and
/// how many turns it may make before it rests again.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.continue.input")]
pub struct ContinueInput {
    /// The session to continue: the seq of the run that opened it.
    session: SessionKey,
    /// The cited session state it continues from. A continue runs a turn only
    /// when this is the session's latest record.
    from: Ref<Session>,
    /// The cited text of the next user message; `None` resends the
    /// conversation as it stands, which only a conversation ending on what a
    /// turn sent allows.
    user: Option<Ref<Utf8Text>>,
    /// Replaces the session's output budget from this turn on; `None` keeps
    /// it.
    max_output_tokens: Option<OutputBudget>,
    /// The most turns the session may make before it rests again.
    max_turns: TurnLimit,
}

impl ContinueInput {
    /// Continue `session` from its record `from` with the user message `user`,
    /// or the conversation as it stands for `None`, and the output budget
    /// `max_output_tokens` from this turn on, or the session's for `None`,
    /// making at most `max_turns` turns before it rests again.
    #[must_use]
    pub const fn new(
        session: SessionKey,
        from: Ref<Session>,
        user: Option<Ref<Utf8Text>>,
        max_output_tokens: Option<OutputBudget>,
        max_turns: TurnLimit,
    ) -> Self {
        Self { session, from, user, max_output_tokens, max_turns }
    }

    /// The session to continue.
    #[must_use]
    pub const fn session(&self) -> SessionKey {
        self.session
    }

    /// The session state it continues from.
    #[must_use]
    pub const fn from(&self) -> Ref<Session> {
        self.from
    }

    /// The most turns the session may make before it rests again.
    #[must_use]
    pub const fn max_turns(&self) -> TurnLimit {
        self.max_turns
    }
}

/// The `muse.session.continue` program.
pub struct SessionContinue;

/// Continues a rested Muse session: its next turn is the session's settings,
/// with the output budget replaced when the input gives one, and its
/// conversation, followed by the user message when the input gives one.
///
/// With no user message the conversation is resent as it stands, which
/// refuses for a conversation that ends on the assistant's reply: only a
/// session that rested failed, or incomplete with no output, ends on what its
/// last turn sent.
///
/// It does not decide whether `from` is the session's latest record; a turn
/// runs only when it is.
#[program]
impl Program for SessionContinue {
    const NAME: &'static str = "muse.session.continue";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Continue a rested Muse session and build its next turn.";
    type Input = ContinueInput;
    type Result = TurnInput;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        env.injected(input.from)?
            .continue_with(input.user, input.max_output_tokens)
            .map_err(|error| Refusal::Refused { reason: Detail::new(format!("the next turn's items: {error}")) })
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Refusal, Tree};
    use aether_data::Ref;

    use super::{ContinueInput, SessionContinue};
    use crate::input::{OfferedTools, OutputBudget, Role, TurnItem};
    use crate::session::fixture::{run, settings, stored};
    use crate::session::state::{RestReason, Session, SessionItems, SessionKey, TurnLimit};

    #[test]
    fn a_continue_builds_the_next_turn_from_the_session_it_names() {
        // Catches a continue that builds its turn from anything but the cited session or drops the input's budget,
        // one that runs without that session in its closure, and one that resends a conversation ending on a reply.
        let items = vec![
            TurnItem::message(Role::User, Ref::of_text("hi")),
            TurnItem::message(Role::Assistant, Ref::of_text("hello")),
        ];
        let items = SessionItems::new(items).expect("items");
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let session = Session::new(settings(OfferedTools::default()), items, RestReason::Completed, tree);
        let from = Ref::of_encoded(&session).expect("session");
        let user = Some(Ref::of_text("more"));
        let budget = Some(OutputBudget::new(4096).expect("budget"));
        let continued =
            |user| ContinueInput::new(SessionKey::new(4), from, user, budget, TurnLimit::new(2).expect("limit"));

        let next = run::<SessionContinue>(&continued(user), vec![stored(&session)]).expect("a named session continues");
        assert_eq!(next, session.continue_with(user, budget).expect("next turn"));
        assert_eq!(run::<SessionContinue>(&continued(user), Vec::new()), Err(Refusal::InputMissing));
        let resent = run::<SessionContinue>(&continued(None), vec![stored(&session)]);
        assert!(matches!(resent, Err(Refusal::Refused { .. })), "a reply is never resent");
    }
}
