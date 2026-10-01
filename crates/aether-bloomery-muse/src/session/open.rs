//! `muse.session.open`: the one way a session starts.

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Tree, Utf8Text};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_bloomery_workspace::TreePath;
use serde_json::Value;

use crate::input::{CallId, ToolCall, ToolCalls, TurnInput};
use crate::session::state::{TurnLimit, TurnSettings};
use crate::session::tools::program_name;
use crate::tools::{READ_MAX_LINES, ReadArgs, TreeRead, offered};

/// A session to open: what every turn sends, the first user message, how
/// many turns it may make before it rests, the tree its tools work on, and
/// the files it reads before its first turn.
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
    /// The tree the session's tools start from. Each call works on the
    /// latest tree, and each record holds it.
    tree: Ref<Tree>,
    /// The files read with `tree.read` before the first turn, in order, at
    /// most [`ToolCalls::MAX_CALLS`]. Each read runs as a call of the
    /// session, so the first turn sends the user message, then every seed's
    /// call, then every seed's output. Empty reads nothing.
    seeds: Vec<TreePath>,
}

impl OpenInput {
    /// Open a session with `settings` on `tree`, starting from the user
    /// message `user` and the reads of `seeds`, that makes at most
    /// `max_turns` turns before it rests.
    #[must_use]
    pub const fn new(
        settings: TurnSettings,
        user: Ref<Utf8Text>,
        max_turns: TurnLimit,
        tree: Ref<Tree>,
        seeds: Vec<TreePath>,
    ) -> Self {
        Self { settings, user, max_turns, tree, seeds }
    }

    /// The most turns the session may make before it rests.
    #[must_use]
    pub const fn max_turns(&self) -> TurnLimit {
        self.max_turns
    }

    /// The tree the session's tools start from.
    #[must_use]
    pub const fn tree(&self) -> Ref<Tree> {
        self.tree
    }

    /// The files read before the first turn, in order.
    #[must_use]
    pub fn seeds(&self) -> &[TreePath] {
        &self.seeds
    }

    /// The first turn: the settings and the user message.
    pub(crate) fn first_turn(&self) -> TurnInput {
        self.settings.open(self.user)
    }
}

/// An opened session: its first turn, and the reads the loop runs before it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.opened")]
pub struct Opened {
    /// The first turn: the settings and the user message.
    turn: Ref<TurnInput>,
    /// The seeded reads the loop runs before that turn, each a decoded
    /// `tree.read` call `seed-<i>`; `None` when there are no seeds.
    seeds: Option<ToolCalls>,
}

impl Opened {
    /// The first turn.
    #[must_use]
    pub const fn turn(&self) -> Ref<TurnInput> {
        self.turn
    }

    /// The seeded reads the loop runs before the first turn, if any.
    #[must_use]
    pub const fn seeds(&self) -> Option<&ToolCalls> {
        self.seeds.as_ref()
    }
}

/// The `muse.session.open` program.
pub struct SessionOpen;

/// Opens a Muse session: its first turn is the settings and the user message,
/// and each seed is a `tree.read` call of the whole window from the file's
/// start, which the loop runs before that turn.
///
/// Refuses settings that offer a tool the session does not bind, or offer a
/// bound tool with a definition, schema, or bound kind other than its own;
/// seeds when `tree.read` is not
/// offered, since a seed's output renders with the offered tool's result
/// schema; and more than [`ToolCalls::MAX_CALLS`] seeds.
#[program]
impl Program for SessionOpen {
    const NAME: &'static str = "muse.session.open";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Open a Muse session and build its first turn and its seeded reads.";
    type Input = OpenInput;
    type Result = Opened;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let (bound, _) = offered();
        if let Some(tool) = input.settings.tools().iter().find(|tool| !bound.offers(tool)) {
            let reason = format!("{} is not offered as a tool the session binds", tool.program().as_str());
            return Err(Refusal::Refused { reason: Detail::new(reason) });
        }
        let seeds = if input.seeds.is_empty() {
            None
        } else {
            Some(seeded(&input, env)?)
        };
        Ok(Opened { turn: env.stage_encoded(&input.first_turn())?, seeds })
    }
}

/// Each seed of `input` as a decoded `tree.read` call `seed-<i>`, its
/// arguments JSON and its arguments staged.
fn seeded(input: &OpenInput, env: &mut Env<Sync>) -> Result<ToolCalls, Refusal> {
    let read = program_name::<TreeRead>();
    if !input.settings.tools().iter().any(|tool| *tool.program() == read) {
        return Err(Refusal::Refused { reason: Detail::new(format!("seeds need {} offered", TreeRead::NAME)) });
    }
    let lines = u32::try_from(READ_MAX_LINES).expect("the most lines one read shows fits a u32");
    let calls = input
        .seeds
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let call_id = CallId::new(format!("seed-{index}")).expect("a seed's call id keeps every call id rule");
            let arguments = env.stage_text(&format!(r#"{{"path":{},"lines":{lines}}}"#, Value::from(path.as_str())));
            let args = env.stage_encoded(&ReadArgs::new(path.clone(), None, Some(lines)))?;
            Ok(ToolCall::decoded(call_id, read.clone(), arguments, args.erase()))
        })
        .collect::<Result<_, Refusal>>()?;
    ToolCalls::new(calls).map_err(|error| Refusal::Refused { reason: Detail::new(format!("the seeds: {error}")) })
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ErasedRef, ProgramName, Ref, Refusal, Tree};
    use aether_bloomery_program::Program;
    use aether_bloomery_workspace::TreePath;
    use aether_data::Kind;

    use super::{OpenInput, SessionOpen};
    use crate::input::tests::offered_tool;
    use crate::input::{OfferedTool, OfferedTools, Role, ToolInput, TurnInput, TurnItem};
    use crate::session::fixture::{path, run_stored, settings};
    use crate::session::state::TurnLimit;
    use crate::tools::{ReadArgs, TreeRead, offered};

    fn open(tools: OfferedTools, seeds: Vec<TreePath>) -> OpenInput {
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        OpenInput::new(settings(tools), Ref::of_text("hi"), TurnLimit::new(4).expect("limit"), tree, seeds)
    }

    #[test]
    fn an_open_is_the_settings_and_the_user_message_over_bound_tools_only() {
        // Catches an open that drops a setting or sends more than the user message, one that admits a tool the
        // loop cannot run, a bound tool offered with another definition or a bound of another kind, one that refuses
        // a bound value it should only check the kind of, and seeds made up from no paths.
        let (bound, _) = offered();
        let (opened, store) = run_stored::<SessionOpen>(&open(bound.clone(), Vec::new())).expect("bound tools open");
        assert_eq!(opened.seeds(), None);
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.settings(), settings(bound.clone()));
        assert_eq!(first.items(), [TurnItem::message(Role::User, Ref::of_text("hi"))]);

        let echo = &bound.as_slice()[0];
        let offer = |definition, bound| {
            OfferedTool::new(echo.program().clone(), definition, echo.input(), bound, echo.result())
        };
        let revalued = offer(echo.definition(), ErasedRef::new(echo.bound().kind(), Ref::of_text("another").digest()));
        let tools = OfferedTools::new(vec![revalued]).expect("tools");
        run_stored::<SessionOpen>(&open(tools, Vec::new()))
            .expect("a bound of the tool's kind opens, whatever its value");

        let redefined = offer(Ref::of_text("{}"), echo.bound());
        let rebound = offer(echo.definition(), ErasedRef::new(Tree::ID, echo.bound().digest()));
        let unbound = offered_tool(ProgramName::new("muse.turn").expect("program"));
        for tool in [redefined, rebound, unbound] {
            let tools = OfferedTools::new(vec![tool]).expect("tools");
            let refused = run_stored::<SessionOpen>(&open(tools, Vec::new()));
            assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
        }
    }

    #[test]
    fn each_seed_is_a_decoded_read_of_its_path_and_seeds_need_the_read_offered() {
        // Catches a seed that reads another path or a partial window, call ids that collide or shift, arguments
        // JSON that does not match the decoded arguments, and seeds opened without `tree.read` to render them.
        let (bound, _) = offered();
        let seeds = vec![path("src/lib.rs"), path("README")];
        let (opened, store) = run_stored::<SessionOpen>(&open(bound.clone(), seeds)).expect("seeds open");
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.items(), [TurnItem::message(Role::User, Ref::of_text("hi"))], "seeds are not in the turn");

        let calls = opened.seeds().expect("seeded reads").as_slice();
        assert_eq!(calls.len(), 2);
        for (index, (call, at)) in calls.iter().zip(["src/lib.rs", "README"]).enumerate() {
            assert_eq!(call.call_id().as_str(), format!("seed-{index}"));
            let ToolInput::Decoded { program, input } = call.input() else {
                panic!("expected seed {index} to decode");
            };
            assert_eq!(program.as_str(), TreeRead::NAME);
            let args: ReadArgs = store.value(input.cast::<ReadArgs>().expect("read arguments"));
            assert_eq!(args, ReadArgs::new(path(at), None, Some(2000)));
            assert_eq!(call.arguments(), Ref::of_text(&format!(r#"{{"path":"{at}","lines":2000}}"#)));
        }

        let unread = OfferedTools::new(vec![bound.as_slice()[0].clone()]).expect("tools");
        let refused = run_stored::<SessionOpen>(&open(unread, vec![path("README")]));
        assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
    }
}
