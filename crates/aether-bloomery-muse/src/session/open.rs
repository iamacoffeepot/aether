//! `muse.session.open`: the one way a session starts.

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};

use crate::input::TurnInput;
use crate::session::state::{TurnLimit, TurnSettings};
use crate::session::tools::offered;

/// A session to open: what every turn sends, the first user message, and how
/// many turns it may make before it rests.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.open.input")]
pub struct OpenInput {
    /// What every turn of the session sends besides its conversation. Every
    /// offered tool must be one the session binds, offered as it renders it.
    settings: TurnSettings,
    /// The cited text of the first user message.
    user: Ref<Utf8Text>,
    /// The most turns the session may make before it rests.
    max_turns: TurnLimit,
}

impl OpenInput {
    /// Open a session with `settings`, starting from the user message `user`,
    /// that makes at most `max_turns` turns before it rests.
    #[must_use]
    pub const fn new(settings: TurnSettings, user: Ref<Utf8Text>, max_turns: TurnLimit) -> Self {
        Self { settings, user, max_turns }
    }

    /// The most turns the session may make before it rests.
    #[must_use]
    pub const fn max_turns(&self) -> TurnLimit {
        self.max_turns
    }
}

/// The `muse.session.open` program.
pub struct SessionOpen;

/// Opens a Muse session: its first turn is the settings and the user message.
///
/// Refuses settings that offer a tool the session does not bind, or offer a
/// bound tool with a definition or schema other than the one it renders.
#[program]
impl Program for SessionOpen {
    const NAME: &'static str = "muse.session.open";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Open a Muse session and build its first turn.";
    type Input = OpenInput;
    type Result = TurnInput;

    fn run(input: Self::Input, _env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let (bound, _) = offered();
        if let Some(tool) = input.settings.tools().iter().find(|tool| !bound.as_slice().contains(tool)) {
            let reason = format!("{} is not offered as a tool the session binds", tool.program().as_str());
            return Err(Refusal::Refused { reason: Detail::new(reason) });
        }
        Ok(input.settings.open(input.user))
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Ref, Refusal};

    use super::{OpenInput, SessionOpen};
    use crate::input::tests::offered_tool;
    use crate::input::{OfferedTool, OfferedTools, Role, TurnItem};
    use crate::session::fixture::{run, settings};
    use crate::session::state::TurnLimit;
    use crate::session::tools::offered;

    #[test]
    fn an_open_is_the_settings_and_the_user_message_over_bound_tools_only() {
        // Catches an open that drops a setting or sends more than the user message, one that admits a tool the
        // loop cannot run, and a bound tool offered with another definition.
        let (bound, _) = offered();
        let user = Ref::of_text("hi");
        let limit = TurnLimit::new(4).expect("limit");

        let first = run::<SessionOpen>(&OpenInput::new(settings(bound.clone()), user, limit), Vec::new())
            .expect("bound tools open");
        assert_eq!(first.settings(), settings(bound.clone()));
        assert_eq!(first.items(), [TurnItem::message(Role::User, user)]);

        let echo = &bound.as_slice()[0];
        let redefined = OfferedTool::new(echo.program().clone(), Ref::of_text("{}"), echo.input(), echo.result());
        let unbound = offered_tool(ProgramName::new("muse.turn").expect("program"));
        for tool in [redefined, unbound] {
            let tools = OfferedTools::new(vec![tool]).expect("tools");
            let refused = run::<SessionOpen>(&OpenInput::new(settings(tools), user, limit), Vec::new());
            assert!(matches!(refused, Err(Refusal::Refused { .. })), "{refused:?}");
        }
    }
}
