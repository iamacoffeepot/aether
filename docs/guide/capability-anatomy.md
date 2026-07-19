# Capability module anatomy

Native capabilities turn privileged host resources into actors. Two accepted
decisions shape their modules:

- ADR-0121: a capability normally owns the kinds it exchanges with callers.
- ADR-0122: its always-addressable identity is separate from state-bearing
  native runtime code.

Use this page to route a change. Use [Adding a chassis capability](recipes/adding-a-chassis-capability.md)
for a worked implementation and current neighboring modules as compile-ready
examples.

## Crate placement

A capability lives either as a module of `aether-capabilities` or as its own
`aether-<cap>` crate. A crate of its own keeps the capability off its
neighbours' reverse-dependency closure, so a change to it reruns only the tests
that actually depend on it. The standalone single caps are `aether-fs`,
`aether-clipboard`, `aether-render`, `aether-trace`, `aether-window`,
`aether-audio`, `aether-inventory`, and `aether-text`. The module shape below is
the same either way. A standalone cap carries its own feature ladder —
`default = ["runtime"]`, with a `runtime` feature gating the substrate-typed
half and the marker face compiling under `default-features = false` — and each
downstream crate depends directly on the cap crate it uses, never through a
re-export facade (a facade would put every downstream back in the cap's
reverse-dependency closure).

A cap that shares infrastructure with a sibling extracts as a small cluster of
interdependent crates rather than one grab-bag crate: a thin foundation crate
holds the shared layer, and each cap crate depends on it. The content-gen
providers are the standing instance — `aether-contentgen` owns the shared
adapter traits, HTTP transport, and `gen/` output staging, and the two provider
caps `aether-anthropic` and `aether-gemini` depend on it. The foundation crate is
a plain dependency of the providers, not a facade over them: nothing re-exports
the cap crates through the foundation, so a provider stays a leaf and only its
own reverse-dependency closure reruns on a change to it.

## Typical directory

```text
aether-capabilities/src/example/
  mod.rs                 identity, public re-exports, feature boundary
  kinds.rs               caller-facing request/reply/value schemas
  config.rs              resolved native config when it is a distinct seam
  runtime.rs             light state + NativeActor handlers
  runtime/               or a directory for a decomposed heavy runtime
  tests.rs               only when tests would overwhelm the subject file
```

Clusters such as `engine/`, `http/`, and `tcp/` keep the root thin and put
independent actors under `server/`, `proxy/`, `listener/`, `session/`, or shard
submodules. Organize by state/lifetime ownership, not an arbitrary line limit.

## Identity in the module root

The marker is a zero-sized type available to callers:

```rust
#[actor(singleton)]
pub struct ExampleCapability;
```

The macro emits addressability, handled-kind markers, and inventory entries.
Guest code can name `ctx.actor::<ExampleCapability>()` without linking native
adapter state.

Use `singleton` for one chassis mailbox and `instanced` for a family whose
runtime discriminator/subname is part of identity. An unsupported chassis may
install a separate headless/fail-fast identity claiming the same public
namespace.

## State in the runtime half

```rust
pub struct ExampleCapabilityState {
    adapter: Arc<dyn ExampleAdapter>,
    pending: HashMap<u64, Pending>,
}

#[runtime]
impl NativeActor for ExampleCapability {
    type State = ExampleCapabilityState;
    type Config = ExampleConfig;

    const NAMESPACE: &'static str = "aether.example";

    fn init(config: ExampleConfig, ctx: &mut NativeInitCtx<'_>)
        -> Result<Self::State, BootError>
    {
        // construct native state
    }

    #[handler::single]
    fn on_request(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        request: Request,
    ) -> ResultKind {
        // serialized actor transition
    }
}
```

The identity type never becomes the resource container. The runtime state owns
devices, adapters, queues, worker handles, and pending correlations.

An empty state is normally a named empty struct rather than hiding the split in
`Self` or an unrelated singleton. Follow the closest current capability because
derive/runtime requirements can vary for test-only actors.

## Kind ownership

Caller-facing `aether.example.*` kinds belong in `example/kinds.rs` and are
re-exported from `example/mod.rs`. The marker face remains wasm-safe: schema,
serde, and lightweight data dependencies only.

Keep a kind in `aether-kinds` only when a named upstream consumer cannot depend
on `aether-capabilities`, or when it is truly substrate-wide. Document that
must-stay reason next to the exception. Current examples include inventory and
some engine/control/capture contracts consumed by MCP or substrate core.

Do not put render/audio/fs/HTTP kinds into the central crate by habit; those
families are capability-owned.

## Feature ladder

Separate four questions:

1. Can a guest name the identity and public kinds?
2. Can native code compile the generic runtime integration?
3. Is a heavy backend linked?
4. Does a particular chassis install/configure the actor?

Common gates are:

- an always-on or light marker feature for transport types;
- `feature = "runtime"` for substrate-typed state and handlers;
- a heavy `<cap>-runtime` feature such as `audio-runtime`, `render-runtime`, or
  `clipboard-runtime` for platform libraries;
- native-target gates for provider/subprocess code that has no useful wasm
  marker face.

Feature presence is not chassis presence. Verify builder composition and live
`describe_handlers` before promising availability.

## Blocking and callback resources

Actor dispatch remains the state owner. Host operations that can block must use
an established boundary:

- sidecar reader/writer/callback threads post bounded events to the actor;
- `dispatch_blocking`/task completion holds settlement and returns results;
- a cap-local queue bounds paid or expensive provider calls;
- shutdown closes/detaches resources without indefinite joins on the dispatcher.

The audio callback is stricter: no allocation, locks, logging, or blocking in the
realtime callback. Network/file/provider work has different adapters but the
same “state transitions return to the actor” rule.

## Configuration

Native configuration is resolved at chassis boot and passed as `type Config`.
Use the repository's derive/layering system for argv, env, and config-file
overlays; do not parse environment variables inside a handler.

Marker-only guest builds usually do not need the resolved config type. Gate and
re-export it at the runtime tier that owns it. A no-config capability can use
`()`, while a named empty config can preserve a likely future composition seam;
follow the neighboring chassis pattern deliberately.

## Fail-fast unsupported actors

If a public mailbox exists conceptually but a chassis cannot provide the
resource, prefer an explicit unsupported actor that replies with the ordinary
error shape. Examples include headless render/window/clipboard companions and
the test-bench unsupported marker.

Do not create a stub for fire-and-forget traffic unless it produces useful
diagnostics and avoids misleading success. The goal is bounded failure, not
pretend capability.

## Tests by boundary

| Contract | Test level |
|---|---|
| kind/schema/validation | unit tests beside `kinds.rs`/validator |
| state machine/adapter mapping | runtime unit tests with a fake adapter |
| actor mail/reply/settlement | focused TestBench test |
| marker/runtime feature split | marker-only build/CI job |
| chassis composition/resource | chassis/TestBench integration test |
| hub/process routing | FleetBench |

Shared RPC test echo code currently lives under
`aether-capabilities/src/rpc/server/test_echo.rs`; engine/proxy tests reuse it.
Do not copy old paths such as a crate-root `test_echo.rs` or `test_chassis.rs`.

## Review checklist

- Identity and kinds are usable without native runtime dependencies.
- Capability-owned kinds did not leak into `aether-kinds` without a reason.
- Runtime state has one actor owner; sidecars return events/results.
- Every request that promises a reply resolves on success, error, disablement,
  and shutdown.
- Queues, bytes, timeouts, retries, and callback work are bounded.
- Every intended chassis installs the real or explicit unsupported actor.
- Resolved config appears in the appropriate `--print-config` surface.
- Live handler/kind inventory sees the contract.
- The selected test crosses the changed boundary.
- A load-bearing new trust/compatibility decision has an ADR.

## Current exemplars

- Light single-file runtime: `fs/`, `input/`, `clipboard/`
- Heavy runtime directory: `audio/`, `render/`, `component/`, `lifecycle/`
- Multi-actor cluster: `engine/`, `http/`, `tcp/`
- Native provider cluster: `anthropic/`, `gemini/`, `shared/contentgen/`
- Split test support: `rpc/server/test_echo.rs`

See [Guest/native boundaries](architecture/guest-native-boundary.md),
[Configuration](systems/configuration.md), and the
[capability index](reference/capability-index.md).
