//! The authoring macros against a live capability.
//!
//! One fixture actor stacks `#[mcp::router]`, `#[http::router]`, and `#[actor]`
//! and carries three tools, two reply mappings sharing one downstream kind, an
//! HTTP route, and its own `RegisterToolResult` handler. Booting it proves what
//! no token-level assertion could: the minted kinds reach the actor dispatcher,
//! the injected `wire` registration is admitted by the real registry, a
//! synchronous tool answers from its own dispatcher, and a deferred one is
//! answered later by the composite handler.
//!
//! The sharp test is the last. `echo_loud` and `echo_soft` defer to the same
//! peer and are answered by the same `EchoResult`, so the only thing that can
//! tell their mappings apart is the `tool_request_kind` carried in the stored
//! `DeferredToolSource`. A handler that selected on the reply kind instead
//! would answer both calls through whichever mapping came first, and the
//! assertion that one call shouts while the other whispers is what catches it.

// The fixture is a deliberate embedder: it builds a bare `TestChassis` through
// `Builder::new` rather than the composed boot seam a production chassis uses,
// and a tool method's return form is the authoring contract the macro parses,
// so a tool whose body cannot fail still declares `Result<Output, ToolError>`.
#![allow(clippy::disallowed_methods, clippy::unnecessary_wraps)] // aether-suppression-request: deliberate test embedder

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use aether_actor::{Manual, actor};
use aether_data::wire;
use aether_http as http;
use aether_http::kinds::HttpServerResponse;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::testing::{TestChassis, fresh_substrate};
use serde::{Deserialize, Serialize};

use crate as mcp;
use crate::McpServerCapability;
use crate::configuration::McpServerConfiguration;
use crate::kinds::{RegisterToolResult, ToolInvocationResult};

/// Wall-clock backstop on every channel wait. It guards in-process mail that
/// answers in single-digit milliseconds, so all it decides is how long a
/// genuinely wedged run takes to fail.
const BACKSTOP: Duration = Duration::from_secs(30);

/// The peer both deferring tools ask.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.echo_request")]
struct EchoRequest {
    text: String,
}

/// The one reply kind that answers two different tools.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.echo_result")]
struct EchoResult {
    text: String,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct SumInput {
    values: Vec<i64>,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct SumOutput {
    total: i64,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct EchoInput {
    text: String,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct LoudOutput {
    shout: String,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct SoftOutput {
    whisper: String,
}

/// The `{ output }` value wrapper, declared independently of the generated one.
///
/// The wire encoding is structural field concatenation, so a locally written
/// wrapper of the same shape decodes the generated one's bytes — which is the
/// assertion: a provider reply carries the output wrapped exactly once, under
/// the field name the capability's registration check requires.
#[derive(Serialize, Deserialize, Debug)]
struct Wrapped<T> {
    output: T,
}

/// Either single-string output, decoded structurally so the two deferring tools
/// are told apart by content rather than by shape.
#[derive(Serialize, Deserialize, Debug)]
struct TextOutput {
    text: String,
}

struct EchoPeer;

#[actor(singleton, root)]
impl NativeActor for EchoPeer {
    type State = ();
    type Config = ();
    const NAMESPACE: &'static str = "aether.mcp.test_echo_peer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<(), BootError> {
        Ok(())
    }

    #[handler::single]
    fn on_echo(_state: &mut (), _ctx: &mut NativeCtx<'_>, request: EchoRequest) -> EchoResult {
        EchoResult { text: request.text }
    }
}

struct ToolProvider;

struct ToolProviderState {
    registrations: Sender<RegisterToolResult>,
}

struct ToolProviderParams {
    registrations: Sender<RegisterToolResult>,
}

#[mcp::router]
#[http::router]
#[actor(singleton, root)]
impl NativeActor for ToolProvider {
    type State = ToolProviderState;
    type Config = ();
    type Params = ToolProviderParams;
    const NAMESPACE: &'static str = "aether.mcp.test_provider";

    fn init((): (), params: ToolProviderParams, _ctx: &mut NativeInitCtx<'_>) -> Result<ToolProviderState, BootError> {
        Ok(ToolProviderState { registrations: params.registrations })
    }

    /// Add the supplied integers.
    #[mcp::tool(
        name = "sum_values",
        title = "Sum values",
        description = "Add the supplied integers and return their total.",
        read_only,
        idempotent,
        closed_world
    )]
    fn sum_values(
        _state: &mut ToolProviderState,
        _context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: SumInput,
    ) -> Result<SumOutput, mcp::ToolError> {
        Ok(SumOutput { total: input.values.iter().sum() })
    }

    /// Echo the text back, shouted.
    #[mcp::tool(name = "echo_loud", description = "Echo the supplied text in upper case.", read_only, closed_world)]
    fn echo_loud(
        _state: &mut ToolProviderState,
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: EchoInput,
    ) -> mcp::Outcome<LoudOutput> {
        context.defer(&EchoRequest { text: input.text }).to::<EchoPeer>()
    }

    /// Echo the text back, whispered.
    #[mcp::tool(name = "echo_soft", description = "Echo the supplied text in lower case.", read_only, closed_world)]
    fn echo_soft(
        _state: &mut ToolProviderState,
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: EchoInput,
    ) -> mcp::Outcome<SoftOutput> {
        context.defer(&EchoRequest { text: input.text }).to::<EchoPeer>()
    }

    #[mcp::reply(EchoResult, tool = echo_loud)]
    fn map_loud(
        _state: &mut ToolProviderState,
        _ctx: &mut NativeCtx<'_, Manual>,
        reply: EchoResult,
    ) -> Result<LoudOutput, mcp::ToolError> {
        Ok(LoudOutput { shout: reply.text.to_uppercase() })
    }

    #[mcp::reply(EchoResult, tool = echo_soft)]
    fn map_soft(
        _state: &mut ToolProviderState,
        _ctx: &mut NativeCtx<'_, Manual>,
        reply: EchoResult,
    ) -> Result<SoftOutput, mcp::ToolError> {
        Ok(SoftOutput { whisper: reply.text.to_lowercase() })
    }

    /// The macro deliberately leaves this slot free so an author can claim it
    /// when registration diagnostics matter. The fixture claims it, which is
    /// how the test sees the injected `wire` sends land.
    #[handler::single]
    fn on_register_tool_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, result: RegisterToolResult) {
        let _ = state.registrations.send(result);
    }

    /// An ordinary HTTP route beside the tools, so the stacked expansion is
    /// proven to leave both surfaces intact on one actor.
    #[http::route(any, "/probe")]
    fn on_probe(_state: &mut ToolProviderState, ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: format!("probe:{}", ctx.request().path).into_bytes(),
        }
    }
}

struct ToolCaller;

struct ToolCallerState {
    answers: Sender<ToolInvocationResult>,
}

struct ToolCallerParams {
    answers: Sender<ToolInvocationResult>,
}

#[actor(singleton, root)]
impl NativeActor for ToolCaller {
    type State = ToolCallerState;
    type Config = ();
    type Params = ToolCallerParams;
    const NAMESPACE: &'static str = "aether.mcp.test_caller";

    fn init((): (), params: ToolCallerParams, _ctx: &mut NativeInitCtx<'_>) -> Result<ToolCallerState, BootError> {
        Ok(ToolCallerState { answers: params.answers })
    }

    /// Invoke all three tools by their minted request kinds — the same kinds
    /// the capability stamps at dispatch, constructed here directly so the test
    /// exercises the generated handlers without standing up the HTTP edge.
    fn wire(_state: &mut ToolCallerState, ctx: &mut NativeCtx<'_>) {
        let provider = ctx.actor::<ToolProvider>();
        provider.send(&ToolProviderModelContextProtocolSumValuesRequest { input: SumInput { values: vec![2, 3, 4] } });
        provider
            .send(&ToolProviderModelContextProtocolEchoLoudRequest { input: EchoInput { text: "Hello".to_owned() } });
        provider
            .send(&ToolProviderModelContextProtocolEchoSoftRequest { input: EchoInput { text: "Hello".to_owned() } });
    }

    #[handler::single]
    fn on_answer(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, answer: ToolInvocationResult) {
        let _ = state.answers.send(answer);
    }
}

/// Boot the capability, the peer, the provider, and the caller, in that order
/// so each actor's `wire` finds what it addresses already live.
fn boot() -> (PassiveChassis<TestChassis>, Receiver<RegisterToolResult>, Receiver<ToolInvocationResult>) {
    let (registry, mailer) = fresh_substrate();
    let (registrations, admitted) = channel();
    let (answers, answered) = channel();

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<McpServerCapability>(
            (),
            McpServerConfiguration { enabled: true, ..McpServerConfiguration::default() },
        )
        .with_actor::<EchoPeer>(())
        .with_actor::<ToolProvider>(ToolProviderParams { registrations })
        .with_actor::<ToolCaller>(ToolCallerParams { answers })
        .build_passive()
        .expect("the fixture chassis boots");

    // The chassis owns the worker pool that pumps every send below, so each
    // test holds it for the duration of its assertions.
    (chassis, admitted, answered)
}

fn collect<T>(source: &Receiver<T>, count: usize, what: &str) -> Vec<T> {
    (0..count)
        .map(|index| {
            source.recv_timeout(BACKSTOP).unwrap_or_else(|error| panic!("{what} {index} never arrived: {error}"))
        })
        .collect()
}

/// The injected `wire` registration reaches the live capability and is admitted
/// for every declared tool.
///
/// It catches a registration that never got injected, one addressed to the
/// wrong capability, and — because the registry recomputes the request
/// identifier from the carried kind name and wrapper schema, and rejects a
/// descriptor whose registrant does not handle that kind — a minted kind whose
/// name, schema carrier, or handler slot disagree with one another.
#[test]
fn injected_registrations_are_admitted_for_every_tool() {
    let (_chassis, admitted, _answered) = boot();

    let results = collect(&admitted, 3, "registration");
    for result in &results {
        assert!(
            matches!(result, RegisterToolResult::Ok),
            "every declared tool registers; the capability refused one: {result:?}",
        );
    }
}

/// A synchronous tool answers from its own dispatcher, wrapped once.
///
/// It catches a dispatcher that dropped the reply obligation, one that replied
/// with the raw output instead of the `{ output }` wrapper the capability
/// decodes against, and an input that was not unwrapped from the minted kind.
#[test]
fn a_result_form_tool_answers_from_its_dispatcher() {
    let (_chassis, _admitted, answered) = boot();

    let sums: Vec<SumOutput> = collect(&answered, 3, "tool answer")
        .iter()
        .filter_map(|answer| match answer {
            ToolInvocationResult::Ok { output_bytes } => {
                wire::from_bytes::<Wrapped<SumOutput>>(output_bytes).ok().map(|wrapped| wrapped.output)
            }
            ToolInvocationResult::Err { .. } => None,
        })
        .collect();

    assert_eq!(sums.len(), 1, "exactly one of the three answers is the summing tool's");
    assert_eq!(sums[0].total, 9, "the dispatcher unwrapped the minted input and wrapped the declared output");
}

/// Two deferring tools sharing one reply kind are answered through their own
/// mappings.
///
/// This is the composite handler's whole reason to exist. Both calls defer to
/// the same peer and come back on the same `EchoResult`, so a handler that
/// selected on the reply kind — or that took the stored context before checking
/// its kind, or that answered the wrong source — would give both calls the same
/// answer. One shout and one whisper is the only outcome that requires
/// selection by `tool_request_kind`.
#[test]
fn deferred_tools_sharing_a_reply_kind_route_by_tool_request_kind() {
    let (_chassis, _admitted, answered) = boot();

    let mut echoes: Vec<String> = collect(&answered, 3, "tool answer")
        .iter()
        .filter_map(|answer| match answer {
            ToolInvocationResult::Ok { output_bytes } => {
                // The summing tool's bytes are a different shape and fail here,
                // which is what leaves exactly the two echo answers.
                wire::from_bytes::<Wrapped<TextOutput>>(output_bytes).ok().map(|wrapped| wrapped.output.text)
            }
            ToolInvocationResult::Err { .. } => None,
        })
        .collect();
    echoes.sort();

    assert_eq!(
        echoes,
        vec!["HELLO".to_owned(), "hello".to_owned()],
        "each deferred call is answered by the mapping bound to its own tool, not by whichever mapping the \
         shared reply kind reached first",
    );
}

/// The peer the composed fixture defers to, answering both of its kinds.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.lookup_request")]
struct LookupRequest {
    key: String,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.lookup_result")]
struct LookupResult {
    value: String,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.tally_request")]
struct TallyRequest {
    count: u32,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.test.tally_result")]
struct TallyResult {
    count: u32,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct LookupInput {
    key: String,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct LookupOutput {
    value: String,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct TallyInput {
    count: u32,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
struct TallyOutput {
    doubled: u32,
}

struct LookupPeer;

#[actor(singleton, root)]
impl NativeActor for LookupPeer {
    type State = ();
    type Config = ();
    const NAMESPACE: &'static str = "aether.mcp.test_lookup_peer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<(), BootError> {
        Ok(())
    }

    #[handler::single]
    fn on_lookup(_state: &mut (), _ctx: &mut NativeCtx<'_>, request: LookupRequest) -> LookupResult {
        LookupResult { value: request.key }
    }

    #[handler::single]
    fn on_tally(_state: &mut (), _ctx: &mut NativeCtx<'_>, request: TallyRequest) -> TallyResult {
        TallyResult { count: request.count }
    }
}

struct ComposedProvider;

struct ComposedProviderState;

/// The two composition forms, on one actor.
///
/// `LookupResult` already has an `#[http::reply]` mapper serving a deferred
/// route; `TallyResult` already has an authored `#[handler::manual]`. Neither
/// gets a second handler: the router composes the tool branch onto the HTTP
/// mapper and injects it into the manual one, which is the whole point of
/// probing the stored context's kind instead of taking it.
#[mcp::router]
#[http::router]
#[actor(singleton, root)]
impl NativeActor for ComposedProvider {
    type State = ComposedProviderState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.mcp.test_composed";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ComposedProviderState, BootError> {
        Ok(ComposedProviderState)
    }

    #[mcp::tool(name = "lookup_value", description = "Look a key up through the peer.", read_only, closed_world)]
    fn lookup_value(
        _state: &mut ComposedProviderState,
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: LookupInput,
    ) -> mcp::Outcome<LookupOutput> {
        context.defer(&LookupRequest { key: input.key }).to::<LookupPeer>()
    }

    #[mcp::tool(name = "tally_count", description = "Double a count through the peer.", read_only, closed_world)]
    fn tally_count(
        _state: &mut ComposedProviderState,
        context: mcp::Context<'_, NativeCtx<'_, Manual>>,
        input: TallyInput,
    ) -> mcp::Outcome<TallyOutput> {
        context.defer(&TallyRequest { count: input.count }).to::<LookupPeer>()
    }

    /// Composed onto: the router consumes both markers, retains this method as
    /// a plain helper, and emits one handler whose fallback calls it and then
    /// `http::answer_deferred`.
    #[mcp::reply(LookupResult, tool = lookup_value, map = map_lookup)]
    #[http::reply]
    fn on_lookup_result(
        _state: &mut ComposedProviderState,
        _ctx: &mut NativeCtx<'_, Manual>,
        result: LookupResult,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: result.value.into_bytes() }
    }

    /// Injected into: this keeps its own handler slot and the tool branch runs
    /// ahead of the authored body.
    #[mcp::reply(TallyResult, tool = tally_count, map = map_tally)]
    #[handler::manual]
    fn on_tally_result(_state: &mut ComposedProviderState, ctx: &mut NativeCtx<'_, Manual>, result: TallyResult) {
        // The injected branches read both bindings ahead of this body, so
        // neither may be underscore-named. Nothing is left to do here: no
        // correlation other than a tool's reaches this fixture's handler.
        let _ = (ctx, result);
    }

    /// A deferred route, so the retained HTTP mapper has an obligation to
    /// answer when no tool claimed the reply.
    #[http::route(any, "/lookup")]
    fn on_lookup_route(_state: &mut ComposedProviderState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        ctx.defer(&LookupRequest { key: "route".to_owned() }).to::<LookupPeer>()
    }
}

impl ComposedProvider {
    fn map_lookup(result: &LookupResult) -> Result<LookupOutput, mcp::ToolError> {
        Ok(LookupOutput { value: result.value.to_uppercase() })
    }

    fn map_tally(result: &TallyResult) -> Result<TallyOutput, mcp::ToolError> {
        Ok(TallyOutput { doubled: result.count * 2 })
    }
}

struct ComposedCaller;

struct ComposedCallerState {
    answers: Sender<ToolInvocationResult>,
}

struct ComposedCallerParams {
    answers: Sender<ToolInvocationResult>,
}

#[actor(singleton, root)]
impl NativeActor for ComposedCaller {
    type State = ComposedCallerState;
    type Config = ();
    type Params = ComposedCallerParams;
    const NAMESPACE: &'static str = "aether.mcp.test_composed_caller";

    fn init(
        (): (),
        params: ComposedCallerParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<ComposedCallerState, BootError> {
        Ok(ComposedCallerState { answers: params.answers })
    }

    fn wire(_state: &mut ComposedCallerState, ctx: &mut NativeCtx<'_>) {
        let provider = ctx.actor::<ComposedProvider>();
        provider.send(&ComposedProviderModelContextProtocolLookupValueRequest {
            input: LookupInput { key: "widget".to_owned() },
        });
        provider.send(&ComposedProviderModelContextProtocolTallyCountRequest { input: TallyInput { count: 21 } });
    }

    #[handler::single]
    fn on_answer(state: &mut ComposedCallerState, _ctx: &mut NativeCtx<'_>, answer: ToolInvocationResult) {
        let _ = state.answers.send(answer);
    }
}

/// Both composition forms answer their deferred tool calls.
///
/// `lookup_value` is answered by a branch composed onto an `#[http::reply]`
/// mapper, `tally_count` by a branch injected into an authored
/// `#[handler::manual]`. Neither reply kind may grow a second handler — the
/// actor dispatcher would refuse that outright — so this compiling at all is
/// half the assertion, and both answers arriving is the other half.
#[test]
fn composed_and_injected_reply_handlers_answer_their_tools() {
    let (registry, mailer) = fresh_substrate();
    let (answers, answered) = channel();

    let _chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<McpServerCapability>(
            (),
            McpServerConfiguration { enabled: true, ..McpServerConfiguration::default() },
        )
        .with_actor::<LookupPeer>(())
        .with_actor::<ComposedProvider>(())
        .with_actor::<ComposedCaller>(ComposedCallerParams { answers })
        .build_passive()
        .expect("the composed fixture chassis boots");

    let mut seen: Vec<String> = Vec::new();
    for answer in collect(&answered, 2, "composed tool answer") {
        let ToolInvocationResult::Ok { output_bytes } = answer else {
            panic!("both composed tools answer successfully: {answer:?}");
        };
        if let Ok(wrapped) = wire::from_bytes::<Wrapped<LookupOutput>>(&output_bytes) {
            seen.push(wrapped.output.value);
        } else if let Ok(wrapped) = wire::from_bytes::<Wrapped<TallyOutput>>(&output_bytes) {
            seen.push(wrapped.output.doubled.to_string());
        }
    }
    seen.sort();

    assert_eq!(
        seen,
        vec!["42".to_owned(), "WIDGET".to_owned()],
        "the composed branch and the injected branch each mapped their own tool's output",
    );
}
