//! `muse.session.open`: the one way a session starts.

use core::num::NonZeroU8;

use aether_bloomery_kinds::{Detail, Mode, Refusal, Tree};
use aether_bloomery_program::{Async, Entropy, Env, Program, program};
use aether_bloomery_workspace::TreePath;
use aether_data::{Ref, Utf8Text};
use serde_json::Value;

use aether_bloomery_workspace_programs::proof::ProofBound;

use crate::input::{CacheKey, CallId, OfferedTool, OfferedTools, ToolCall, ToolCalls, TurnInput};
use crate::session::gate::{RequiredProof, RequiredProofs};
use crate::session::state::{TurnLimit, TurnSettings};
use crate::session::tools::program_name;
use crate::tools::{READ_MAX_LINES, ReadArgs, TreeDiff, TreeRead, offered, proof_bound_offers, proof_offers};

/// A session to open: what every turn sends, the session instructions, the
/// first user message, how many turns it may make before it rests, the tree
/// its tools work on, the files it reads before its first turn, and the
/// proofs a `Done` end must pass.
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
    /// The proofs a `Done` end must pass on the session's tree, in the order
    /// the gate runs them. Each must be offered as a proof tool, with
    /// arguments of the kind its offer's input schema names. Chosen by the
    /// opener and never sent to the model; empty gates nothing.
    required: RequiredProofs,
    /// The prompt cache key the session shares with every other session
    /// opened with it; `None` draws one fresh for this session.
    shared_key: Option<CacheKey>,
}

impl OpenInput {
    /// Open a session with `settings` on `tree`, starting from `instructions`,
    /// the user message `user`, and the reads of `seeds`, that makes at most
    /// `max_turns` turns before it rests, and whose `Done` end must pass
    /// `required`.
    #[must_use]
    pub const fn new(
        settings: TurnSettings,
        instructions: Ref<Utf8Text>,
        user: Ref<Utf8Text>,
        max_turns: TurnLimit,
        tree: Ref<Tree>,
        seeds: Vec<TreePath>,
        required: RequiredProofs,
    ) -> Self {
        Self { settings, instructions, user, max_turns, tree, seeds, required, shared_key: None }
    }

    /// Share `key` as this session's prompt cache key with every other session
    /// opened with it. The one way sessions come to share a prompt cache.
    #[must_use]
    pub fn sharing(self, key: CacheKey) -> Self {
        Self { shared_key: Some(key), ..self }
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

    /// The proofs a `Done` end must pass, in the order the gate runs them.
    #[must_use]
    pub const fn required(&self) -> &RequiredProofs {
        &self.required
    }

    /// The first turn: the settings with the instructions as the leading
    /// developer message and the user message, sent with `cache_key`.
    pub(crate) fn first_turn(&self, cache_key: CacheKey) -> TurnInput {
        self.settings.open(self.instructions, self.user, cache_key)
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
    /// The key the first turn sends: drawn by the open, or the one it shares.
    cache_key: CacheKey,
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

    /// The key the first turn sends.
    #[must_use]
    pub const fn cache_key(&self) -> &CacheKey {
        &self.cache_key
    }
}

/// The `muse.session.open` program.
pub struct SessionOpen;

/// Opens a Muse session: it draws the session's prompt cache key, unless the
/// opener named one to share, and its first turn is the settings with the
/// instructions as the leading developer message and the user message, sent
/// with that key. Each seed is a `tree.read` call of the whole window from the
/// file's start, which the loop runs before that turn.
///
/// Refuses settings that offer a tool the session does not bind, or offer a
/// bound tool with a definition, schema, bundle head, or bound kind other than
/// its own; the proof tools and the vendor view bind a `ProofBound`, the
/// session's environment, vendor tree, and test env, whatever its value.
/// Refuses a `tree.diff` offer bound to a tree other than the session's.
/// Refuses a required proof that is not offered as a proof tool, since the
/// gate runs it through its offer, and required arguments of another kind
/// than the offer's input schema names, which the proof could not decode; a
/// vendor tool is no proof. Refuses seeds when `tree.read` is not offered,
/// since a seed's output renders with the offered tool's result schema; and
/// more than [`ToolCalls::MAX_CALLS`] seeds.
#[program]
impl Program for SessionOpen {
    const NAME: &'static str = "muse.session.open";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Open a Muse session and build its first turn and its seeded reads.";
    type Input = OpenInput;
    type Result = Opened;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut entropy: Entropy) -> Result<Self::Result, Refusal> {
        let (own, _) = offered(input.tree());
        let unbound = input.settings.tools().iter().find(|tool| !binds(&own, tool));
        if let Some(tool) = unbound {
            let reason = format!("{} is not offered as a tool the session binds", tool.program().as_str());
            return Err(Refusal::Refused { reason: Detail::new(reason) });
        }
        let rebased = input.settings.tools().iter().any(|tool| diffs_another_tree(tool, input.tree()));
        if rebased {
            return Err(refused(String::from("tree.diff is bound to a tree other than the session's")));
        }
        for proof in input.required.as_slice() {
            gates(&input, proof, env)?;
        }

        let cache_key = if let Some(key) = input.shared_key.clone() {
            key
        } else {
            CacheKey::drawn(
                &entropy
                    .draw(NonZeroU8::new(16).ok_or_else(|| refused(String::from("entropy draw needs a count")))?)
                    .await?,
            )
            .map_err(|error| refused(format!("the drawn cache key: {error}")))?
        };
        let seeds = if input.seeds.is_empty() {
            None
        } else {
            Some(seeded(&input, &mut env)?)
        };
        let turn = env.stage_encoded(&input.first_turn(cache_key.clone()))?;
        Ok(Opened { turn, seeds, cache_key })
    }
}

/// Whether the session binds `tool`: one of `own`, or a proof tool or vendor
/// tool offered over a `ProofBound`, whatever its value.
fn binds(own: &OfferedTools, tool: &OfferedTool) -> bool {
    let is_own = own.offers(tool);
    let is_proof_bound = proof_bound(tool);
    is_own || is_proof_bound
}

/// Whether the tool is the diff tool bound to a tree other than the base.
fn diffs_another_tree(tool: &OfferedTool, base: Ref<Tree>) -> bool {
    let is_diff = *tool.program() == program_name::<TreeDiff>();
    let rebased = tool.bound() != base.erase();
    is_diff && rebased
}

/// Whether `tool` is a proof tool or a vendor tool offered over a
/// `ProofBound`, whatever its value.
fn proof_bound(tool: &OfferedTool) -> bool {
    let offers = tool.bound().cast::<ProofBound>().map(proof_bound_offers).unwrap_or_default();
    offers.iter().any(|(offer, _)| offer.same_tool(tool))
}

/// Whether `tool` is a proof tool offered over a `ProofBound`, whatever its
/// value.
fn proves(tool: &OfferedTool) -> bool {
    let proof = tool.bound().cast::<ProofBound>().map(proof_offers).unwrap_or_default();
    proof.iter().any(|offer| offer.tool.same_tool(tool))
}

/// Refuses `proof` unless `input` offers its program as a proof tool and its
/// arguments are of the kind that offer's input schema names.
fn gates(input: &OpenInput, proof: &RequiredProof, env: Env<Async>) -> Result<(), Refusal> {
    let program = proof.program().as_str();
    let offer =
        input.settings.tools().iter().find(|tool| tool.program() == proof.program()).filter(|tool| proves(tool));
    let Some(offer) = offer else {
        return Err(refused(format!("{program} is required but not offered as a proof tool")));
    };
    let schema = env.injected(offer.input())?;
    let fits = proof.args().kind() == schema.kind_id();
    if !fits {
        return Err(refused(format!("{program} is required with arguments that are not a {}", schema.kind_name())));
    }
    Ok(())
}

fn refused(reason: String) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}

/// Each seed of `input` as a decoded `tree.read` call `seed-<i>`, its
/// arguments JSON and its arguments staged.
fn seeded(input: &OpenInput, env: &mut Env<Async>) -> Result<ToolCalls, Refusal> {
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
    use aether_bloomery_workspace_programs::proof::{ClippyProof, ProofBound};
    use aether_data::{Digest, ErasedRef, Kind, Ref};

    use super::{OpenInput, SessionOpen};
    use crate::input::tests::offered_tool;
    use crate::input::{CacheKey, OfferedTool, OfferedTools, Role, ToolInput, TurnInput, TurnItem};
    use crate::session::MUSE;
    use crate::session::fixture::{path, run_async, run_drawn, settings};
    use crate::session::gate::RequiredProofs;
    use crate::session::state::TurnLimit;
    use crate::tools::{ReadArgs, TreeRead, VendorGrep, VendorList, VendorRead, offered, offered_with_proofs};

    fn open(tools: OfferedTools, seeds: Vec<TreePath>) -> OpenInput {
        open_requiring(tools, seeds, RequiredProofs::default())
    }

    fn open_requiring(tools: OfferedTools, seeds: Vec<TreePath>, required: RequiredProofs) -> OpenInput {
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        OpenInput::new(
            settings(tools),
            Ref::of_text("rules"),
            Ref::of_text("hi"),
            TurnLimit::new(4).expect("limit"),
            tree,
            seeds,
            required,
        )
    }

    fn drawn(bytes: &[u8]) -> CacheKey {
        CacheKey::drawn(bytes).expect("drawn key")
    }

    #[test]
    fn an_open_is_the_settings_and_the_opening_messages_over_bound_tools_only() {
        // Catches an open that drops a setting or the instructions or swaps them, sends more than the two opening
        // messages, one that admits a tool the loop cannot run, a bound tool offered with another definition or a
        // bound of another kind or from another bundle head, one that refuses a bound value it should only check the kind of, and seeds made up
        // from no paths.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (bound, _) = offered(tree);
        let (opened, store) =
            run_drawn::<SessionOpen>(&open(bound.clone(), Vec::new()), Vec::new(), &[7; 16]).expect("bound tools open");
        assert_eq!(opened.seeds(), None);
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.settings(), settings(bound.clone()));
        assert_eq!(first.cache_key(), &drawn(&[7; 16]));
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
        run_drawn::<SessionOpen>(&open(tools, Vec::new()), Vec::new(), &[7; 16])
            .expect("a bound of the tool's kind opens, whatever its value");

        let redefined = offer(MUSE, Ref::of_text("{}"), echo.bound());
        let rebound = offer(MUSE, echo.definition(), ErasedRef::new(Tree::ID, echo.bound().digest()));
        let rehomed = offer(Head::new("proofs"), echo.definition(), echo.bound());
        let unbound = offered_tool(ProgramName::new("muse.turn").expect("program"));
        for tool in [redefined, rebound, rehomed, unbound] {
            let tools = OfferedTools::new(vec![tool]).expect("tools");
            let refused = run_drawn::<SessionOpen>(&open(tools, Vec::new()), Vec::new(), &[7; 16]);
            assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
        }
    }

    #[test]
    fn a_diff_bound_to_another_tree_is_refused() {
        // Catches an open that checks the bound's kind only, which would let a diff run against a tree the session
        // never opened on.
        use std::iter::once;

        use aether_bloomery_kinds::Name;
        use aether_bloomery_kinds::Node;

        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let other_root =
            Tree::new(once((Name::new("other").expect("name"), Node::File(Ref::of_bytes(b"other")))).collect());
        let other = Ref::of_encoded(&other_root).expect("other");
        let (other_offer, _) = offered(other);
        let refused = run_drawn::<SessionOpen>(&open(other_offer, Vec::new()), Vec::new(), &[7; 16]);
        assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
        let (same_offer, _) = offered(tree);
        run_drawn::<SessionOpen>(&open(same_offer, Vec::new()), Vec::new(), &[7; 16])
            .expect("a diff bound to the session's tree opens");
    }

    #[test]
    fn proof_bound_offers_open_over_any_proof_bound_from_their_own_bundle_only() {
        // Catches an open that refuses the proof tools or the vendor view a lane offers, admits one whose bound the
        // tool cannot decode or that the loop would call in a bundle that does not hold it, or opens over only one
        // of the offered tools.
        use aether_bloomery_workspace_programs::proof::TestEnv;

        use crate::tools::proof_bound_offers;

        let digest = |byte| Digest::from_bytes([byte; 32]);
        let proofs = ProofBound::new(Ref::from_digest(digest(1)), Ref::from_digest(digest(2)), TestEnv::default());
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (tools, _) = offered_with_proofs(tree, &proofs);
        run_drawn::<SessionOpen>(&open(tools.clone(), Vec::new()), Vec::new(), &[7; 16])
            .expect("bound proof tools open");

        for (offer, _) in proof_bound_offers(Ref::from_digest(digest(1))) {
            let single = OfferedTools::new(vec![offer]).expect("tools");
            run_drawn::<SessionOpen>(&open(single, Vec::new()), Vec::new(), &[7; 16])
                .expect("every proof-bound offer opens");
        }

        let other = ProofBound::new(Ref::from_digest(digest(3)), Ref::from_digest(digest(4)), TestEnv::default());
        let other = Ref::of_encoded(&other).expect("bound").erase();
        let offered_as = |name: &str| {
            tools
                .as_slice()
                .iter()
                .find(|tool| tool.program().as_str() == name)
                .unwrap_or_else(|| panic!("{name} is offered"))
        };
        for (name, home, other_home) in [
            (ClippyProof::NAME, WORKSPACE_PROGRAMS, MUSE),
            (VendorList::NAME, MUSE, WORKSPACE_PROGRAMS),
            (VendorRead::NAME, MUSE, WORKSPACE_PROGRAMS),
            (VendorGrep::NAME, MUSE, WORKSPACE_PROGRAMS),
        ] {
            let tool = offered_as(name);
            let offer = |head, bound| {
                OfferedTool::new(tool.program().clone(), head, tool.definition(), tool.input(), bound, tool.result())
            };
            let rebound = offer(home.clone(), other);
            run_drawn::<SessionOpen>(
                &open(OfferedTools::new(vec![rebound]).expect("tools"), Vec::new()),
                Vec::new(),
                &[7; 16],
            )
            .unwrap_or_else(|refused| panic!("{name} opens over any proof bound: {refused:?}"));

            let foreign = offer(home, ErasedRef::new(Tree::ID, tool.bound().digest()));
            let rehomed = offer(other_home, tool.bound());
            for tool in [foreign, rehomed] {
                let tools = OfferedTools::new(vec![tool]).expect("tools");
                let refused = run_drawn::<SessionOpen>(&open(tools, Vec::new()), Vec::new(), &[7; 16]);
                assert!(matches!(refused, Err(Refusal::Refused { .. })), "{name}: {:?}", refused.err());
            }
        }
    }

    #[test]
    fn a_required_proof_must_be_offered_as_a_proof_with_arguments_its_offer_decodes() {
        // Catches a gate the model could dodge because the required proof is not offered so nothing runs, a
        // required tool that is no proof (a vendor tool offered over the same bound included), and required
        // arguments the proof cannot decode as its input.
        use aether_bloomery_kinds::ClosureArtifact;
        use aether_bloomery_workspace_programs::proof::{ClippyArgs, TestArgs, TestEnv};

        use crate::session::gate::RequiredProof;
        use crate::tools::EchoArgs;

        let digest = |byte| Digest::from_bytes([byte; 32]);
        let proofs = ProofBound::new(Ref::from_digest(digest(1)), Ref::from_digest(digest(2)), TestEnv::default());
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (proving, artifacts) = offered_with_proofs(tree, &proofs);
        let closure: Vec<_> = artifacts
            .into_iter()
            .map(|artifact| {
                let (kind, payload, _) = artifact.into_parts();
                ClosureArtifact::new(kind, payload)
            })
            .collect();
        let required = |program: &str, args| {
            let program = ProgramName::new(program).expect("program");
            RequiredProofs::new(vec![RequiredProof::new(program, args)]).expect("required")
        };
        let clippy_args = Ref::of_encoded(&ClippyArgs).expect("args").erase();
        let opens = |tools: &OfferedTools, required| {
            run_drawn::<SessionOpen>(&open_requiring(tools.clone(), Vec::new(), required), closure.clone(), &[7; 16])
                .map(|(opened, _)| opened)
        };

        opens(&proving, required("proof.clippy", clippy_args)).expect("a required proof over its offer opens");

        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (own, _) = offered(tree);
        let echo_args = Ref::of_encoded(&EchoArgs::new("hi")).expect("args").erase();
        let test_args = Ref::of_encoded(&TestArgs).expect("args").erase();
        let read_args = Ref::of_encoded(&ReadArgs::new(path("README"), None, None)).expect("args").erase();
        for (tools, required) in [
            (&own, required("proof.clippy", clippy_args)),
            (&proving, required("muse.echo", echo_args)),
            (&proving, required("vendor.read", read_args)),
            (&proving, required("proof.clippy", test_args)),
        ] {
            let refused = opens(tools, required);
            assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
        }
    }

    #[test]
    fn each_seed_is_a_decoded_read_of_its_path_and_seeds_need_the_read_offered() {
        // Catches a seed that reads another path or a partial window, call ids that collide or shift, arguments
        // JSON that does not match the decoded arguments, and seeds opened without `tree.read` to render them.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (bound, _) = offered(tree);
        let seeds = vec![path("src/lib.rs"), path("README")];
        let (opened, store) =
            run_drawn::<SessionOpen>(&open(bound.clone(), seeds), Vec::new(), &[7; 16]).expect("seeds open");
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
        let refused = run_drawn::<SessionOpen>(&open(unread, vec![path("README")]), Vec::new(), &[7; 16]);
        assert!(matches!(refused, Err(Refusal::Refused { .. })), "{:?}", refused.err());
    }

    #[test]
    fn an_open_draws_its_cache_key_from_entropy() {
        // Catches a shared key made up when none was named, which would route
        // sessions with the same opening together.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (bound, _) = offered(tree);
        let bytes = [9, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67];
        let (opened, store) =
            run_drawn::<SessionOpen>(&open(bound, Vec::new()), Vec::new(), &bytes).expect("an open draws");
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.cache_key(), &drawn(&bytes));
    }

    #[test]
    fn two_opens_fed_different_bytes_get_different_keys() {
        // Catches a drawn key that ignores its bytes, which would share a cache
        // across sessions no one asked to share.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (bound, _) = offered(tree);
        let (first, first_store) =
            run_drawn::<SessionOpen>(&open(bound.clone(), Vec::new()), Vec::new(), &[1; 16]).expect("first open draws");
        let (second, second_store) =
            run_drawn::<SessionOpen>(&open(bound, Vec::new()), Vec::new(), &[2; 16]).expect("second open draws");
        let first_turn: TurnInput = first_store.value(first.turn());
        let second_turn: TurnInput = second_store.value(second.turn());
        assert_ne!(first_turn.cache_key(), second_turn.cache_key());
    }

    #[test]
    fn a_shared_key_opens_on_the_named_key_without_drawing() {
        // Catches a shared open that draws anyway, or sends another key than
        // the named one.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (bound, _) = offered(tree);
        let shared = CacheKey::new("shared-cache").expect("shared key");
        let input = open(bound, Vec::new()).sharing(shared.clone());
        let (opened, store) = run_async::<SessionOpen>(&input, Vec::new()).expect("a shared open draws nothing");
        let first: TurnInput = store.value(opened.turn());
        assert_eq!(first.cache_key(), &shared);
    }
}
