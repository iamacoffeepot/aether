//! `muse.session.continue`: the one way a rested session takes its next turn.

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};

use crate::input::TurnInput;
use crate::session::state::{Session, SessionKey, TurnLimit};

/// A rested session to continue: which session, the [`Session`] it continues
/// from, the next user message, and how many turns it may make before it rests
/// again.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.continue.input")]
pub struct ContinueInput {
    /// The session to continue: the seq of the run that opened it.
    session: SessionKey,
    /// The cited session state it continues from. A continue runs a turn only
    /// when this is the session's latest record.
    from: Ref<Session>,
    /// The cited text of the next user message.
    user: Ref<Utf8Text>,
    /// The most turns the session may make before it rests again.
    max_turns: TurnLimit,
}

impl ContinueInput {
    /// Continue `session` from its record `from` with the user message `user`,
    /// making at most `max_turns` turns before it rests again.
    #[must_use]
    pub const fn new(session: SessionKey, from: Ref<Session>, user: Ref<Utf8Text>, max_turns: TurnLimit) -> Self {
        Self { session, from, user, max_turns }
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

/// Continues a rested Muse session: its next turn is the session's settings
/// and conversation followed by the user message.
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
            .continue_with(input.user)
            .map_err(|error| Refusal::Refused { reason: Detail::new(format!("the next turn's items: {error}")) })
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Ref, Refusal, Tree};

    use super::{ContinueInput, SessionContinue};
    use crate::input::{OfferedTools, Role, TurnItem};
    use crate::session::fixture::{run, settings, stored};
    use crate::session::state::{RestReason, Session, SessionItems, SessionKey, TurnLimit};

    #[test]
    fn a_continue_builds_the_next_turn_from_the_session_it_names() {
        // Catches a continue that builds its turn from anything but the cited session, and one that runs without
        // that session in its closure.
        let items = vec![
            TurnItem::message(Role::User, Ref::of_text("hi")),
            TurnItem::message(Role::Assistant, Ref::of_text("hello")),
        ];
        let items = SessionItems::new(items).expect("items");
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let session = Session::new(settings(OfferedTools::default()), items, RestReason::Completed, tree);
        let user = Ref::of_text("more");
        let input = ContinueInput::new(
            SessionKey::new(4),
            Ref::of_encoded(&session).expect("session"),
            user,
            TurnLimit::new(2).expect("limit"),
        );

        let next = run::<SessionContinue>(&input, vec![stored(&session)]).expect("a named session continues");
        assert_eq!(next, session.continue_with(user).expect("next turn"));
        assert_eq!(run::<SessionContinue>(&input, Vec::new()), Err(Refusal::InputMissing));
    }
}
