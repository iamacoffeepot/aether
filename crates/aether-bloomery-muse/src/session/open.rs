//! `muse.session.open`: the one way a session starts.

use aether_bloomery_kinds::{Detail, Mode, Refusal, Tree};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_bloomery_workspace::TreePath;
use aether_data::{Ref, Utf8Text};
use serde_json::Value;

use aether_bloomery_workspace_programs::proof::ProofBound;

use crate::input::{CallId, OfferedTool, OfferedTools, ToolCall, ToolCalls, TurnInput};
use crate::session::state::{TurnLimit, TurnSettings};
use crate::session::tools::program_name;
use crate::tools::{READ_MAX_LINES, ReadArgs, TreeRead, offered, proof_offers};

/// A session to open: what every turn sends, the session instructions, the
/// first user message, how many turns it may make before it rests, the tree
/// its tools work on, and the files it reads before its first turn.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.open.input")]
pub struct OpenInput {
    /// What every turn of the session sends besides its conversation. Every
    /// offered tool must be one the session binds, offered as it renders it.
    settings: TurnSettings,
    /// The cited text of the session instructions, sent as the leading
    /// developer message of the first turn.
    instructions: Ref<Utf8Text>,
    /// The cited text of the first user message.
    user: Ref<Utf8Text>,
    /// The most turns the session may make before it rests.
    max_turns: TurnLimit,
    /// The tree the session's tools start from. Each call works on the
    /// latest tree, and each record holds it.
    tree: Ref<Tree>,
    /// The files read with `tree.read` before the first turn, in order, at
    /// most [`ToolCalls::MAX_CALLS`]. Each read runs as a call of the
    /// session, so the first turn sends the instructions, the user message,
    /// then every seed's call, then every seed's output. Empty reads nothing.
    seeds: Vec<TreePath>,
}

impl OpenInput {
    /// Open a session with `settings` on `tree`, starting from `instructions`,
    /// the user message `user`, and the reads of `seeds`, that makes at most
    /// `max_turns` turns before it rests.
    #[must_use]
    pub const fn new(
        settings: TurnSettings,
        instructions: Ref<Utf8Text>,
        user: Ref<Utf8Text>,
        max_turns: TurnLimit,
        tree: Ref<Tree>,
        seeds: Vec<TreePath>,
    ) -> Self {
        Self { settings, instructions, user, max_turns, tree, seeds }
    }

    /// The cited text of the session instructions.
    #[must_use]
    pub const fn instructions(&self) -> Ref<Utf8Text> {
        self.instructions
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

    /// The first turn: the settings with the instructions as the leading
    /// developer message and the user message.
    pub(crate) fn first_turn(&self) -> TurnInput {
        self.settings.open(self.instructions, self.user)
    }
}

/// An opened session: its first turn, and the reads the loop runs before it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.opened")]
pub struct Opened {
    /// The first turn: the settings with the instructions as the leading
    /// developer message and the user message.
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

/// Opens a Muse session: its first turn is the settings with the instructions
/// as the leading developer message and the user message, and each seed is a
/// `tree.read` call of the whole window from the file's start, which the loop
/// runs before that turn.
///
/// Refuses settings that offer a tool the session does not bind, or offer a
/// bound tool with a definition, schema, bundle head, or bound kind other than
/// its own; the proof tools bind a `ProofBound`, the session's environment,
/// vendor tree, and test env, whatever its value. Refuses seeds when `tree.read` is not
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
        let (own, _) = offered();
        let unbound = input.settings.tools().iter().find(|tool| !binds(&own, tool));
        if let Some(tool) = unbound {
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

/// Whether the session binds `tool`: one of `own`, or a proof tool offered
/// over a `ProofBound`, whatever its value.
fn binds(own: &OfferedTools, tool: &OfferedTool) -> bool {
    let proof = tool.bound().cast::<ProofBound>().map(proof_offers).unwrap_or_default();
    let is_own = own.offers(tool);
    let is_proof = proof.iter().any(|(offer, _)| offer.same_tool(tool));
    is_own || is_proof
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
    use aether_bloomery_kinds::{Head, ProgramName, Refusal, Tree};
    use aether_bloomery_program::Program;
    use aether_bloomery_workspace::TreePath;
    use aether_bloomery_workspace_programs::WORKSPACE_PROGRAMS;
    use aether_bloomery_workspace_programs::proof::ProofBound;
    use aether_data::{Digest, ErasedRef, Kind, Ref};

    use super::{OpenInput, SessionOpen};
    use crate::input::tests::offered_tool;
    use crate::input::{OfferedTool, OfferedTools, Role, ToolInput, TurnInput, TurnItem};
    use crate::session::MUSE;
    use crate::session::fixture::{path, run_stored, settings};
    use crate::session::state::TurnLimit;
    use crate::tools::{ReadArgs, TreeRead, offered, offered_with_proofs};

    fn open(tools: OfferedTools, seeds: Vec<TreePath>) -> OpenInput {
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        OpenInput::new(
            settings(tools),
            Ref::of_text("rules"),
            Ref::of_text("hi"),
            TurnLimit::new(4).expect("limit"),
            tree,
            seeds,
        )
    }

    #[test]
    fn an_open_is_the_settings_and_the_opening_messages_over_bound_tools_only() {
        // Catches an open that drops a setting or the instructions or swaps them, sends more than the two opening
        // messages, one that admits a tool the loop cannot run, a bound tool offered with another definition or a
        // bound of another kind or from another bundle head, one that refuses a bound value it should only check the kind of, and seeds made up
        // from no paths.
        let (bound, _) = offered();
        let (opened, store) = run_stored::<SessionOpen>(&open(bound.clone(), Vec::new())).expect("bound tools open");
        assert_eq!(opened.seeds(), None);
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.settings(), settings(bound.clone()));
        assert_eq!(
            first.items(),
            [
                TurnItem::message(Role::Developer, Ref::of_text("rules")),
                TurnItem::message(Role::User, Ref::of_text("hi"))
            ]
        );

        let echo = &bound.as_slice()[0];
        let offer = |head, definition, bound| {
            OfferedTool::new(echo.program().clone(), head, definition, echo.input(), bound, echo.result())
        };
        let revalued =
            offer(MUSE, echo.definition(), ErasedRef::new(echo.bound().kind(), Ref::of_text("another").digest()));
        let tools = OfferedTools::new(vec![revalued]).expect("tools");
        run_stored::<SessionOpen>(&open(tools, Vec::new()))
            .expect("a bound of the tool's kind opens, whatever its value");

        let redefined = offer(MUSE, Ref::of_text("{}"), echo.bound());
        let rebound = offer(MUSE, echo.definition(), ErasedRef::new(Tree::ID, echo.bound().digest()));
        let rehomed = offer(Head::new("proofs"), echo.definition(), echo.bound());
        let unbound = offered_tool(ProgramName::new("muse.turn").expect("program"));
        for tool in [redefined, rebound, rehomed, unbound] {
            let tools = OfferedTools::new(vec![tool]).expect("tools");
            let refused = run_stored::<SessionOpen>(&open(tools, Vec::new()));
            assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
        }
    }

    #[test]
    fn proof_offers_open_over_any_proof_bound_from_their_own_bundle_only() {
        // Catches an open that refuses the proof tools a lane offers, admits one whose bound the proof cannot
        // decode or that the loop would call in a bundle that does not hold it, or opens over only one of the
        // offered proofs.
        use aether_bloomery_workspace_programs::proof::TestEnv;

        use crate::tools::proof_offers;

        let digest = |byte| Digest::from_bytes([byte; 32]);
        let proofs = ProofBound::new(Ref::from_digest(digest(1)), Ref::from_digest(digest(2)), TestEnv::default());
        let (tools, _) = offered_with_proofs(&proofs);
        run_stored::<SessionOpen>(&open(tools.clone(), Vec::new())).expect("bound proof tools open");

        for (offer, _) in proof_offers(Ref::from_digest(digest(1))) {
            let single = OfferedTools::new(vec![offer]).expect("tools");
            run_stored::<SessionOpen>(&open(single, Vec::new())).expect("every proof offer opens");
        }

        let clippy = tools.as_slice().last().expect("the proof is offered last");
        let offer = |head, bound| {
            OfferedTool::new(
                clippy.program().clone(),
                head,
                clippy.definition(),
                clippy.input(),
                bound,
                clippy.result(),
            )
        };
        let other = ProofBound::new(Ref::from_digest(digest(3)), Ref::from_digest(digest(4)), TestEnv::default());
        let rebound = offer(WORKSPACE_PROGRAMS, Ref::of_encoded(&other).expect("bound").erase());
        run_stored::<SessionOpen>(&open(OfferedTools::new(vec![rebound]).expect("tools"), Vec::new()))
            .expect("a proof opens over any proof bound");

        let foreign = offer(WORKSPACE_PROGRAMS, ErasedRef::new(Tree::ID, clippy.bound().digest()));
        let rehomed = offer(MUSE, clippy.bound());
        for tool in [foreign, rehomed] {
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
        assert_eq!(
            first.items(),
            [
                TurnItem::message(Role::Developer, Ref::of_text("rules")),
                TurnItem::message(Role::User, Ref::of_text("hi"))
            ],
            "seeds are not in the turn"
        );

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
