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

A capability lives in its own `aether-<cap>` crate, named for the mailbox it
owns. A crate of its own keeps the capability off its neighbours'
reverse-dependency closure, so a change to it reruns only the tests that
actually depend on it. A cap crate normally carries its own feature ladder —
`default = ["runtime"]`, with a `runtime` feature gating the substrate-typed
half and the marker face compiling under `default-features = false`; a
native-only cap no guest can address is exempt (see
[Feature ladder](#feature-ladder)) — and each
downstream crate depends directly on the cap crate it uses, never through a
re-export facade (a facade would put every downstream back in the cap's
reverse-dependency closure).

A provider that shares pure logic with a sibling factors that logic into a thin
foundation crate the provider depends on rather than a grab-bag crate — a plain
dependency, not a facade: nothing re-exports the provider through the
foundation, so the provider stays a leaf and only its own reverse-dependency
closure reruns on a change to it. Reach for the foundation crate only once the
logic is genuinely shared; a content-gen provider keeps its own wasm-safe
DTO and string helpers rather than sharing a one-consumer crate, so
`aether-anthropic` is a self-contained leaf. (It is a wasm guest component
loaded on demand, not a native chassis capability — ADR-0159.)

## Typical directory

```text
aether-<cap>/src/
  lib.rs                 crate root: identity, public re-exports, feature boundary
  module/mod.rs          optional module root for a decomposed subsystem
  kinds.rs               caller-facing request/reply/value schemas
  config.rs              resolved native config when it is a distinct seam
  runtime.rs             light state + NativeActor handlers
  runtime/               or a directory for a decomposed heavy runtime
  tests.rs               only when tests would overwhelm the subject file
```

Clusters such as `aether-fleet`, `aether-http`, and `aether-tcp` keep the root
thin and put independent actors under `server/`, `proxy/`, `listener/`,
`session/`, or shard submodules. Organize by state/lifetime ownership, not an arbitrary line limit.

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

Runtime-placement paths let multiple implementations live beside one public
identity without forcing directory names into actor identity. Window is the
multi-runtime exemplar:

```rust
#[actor(singleton)]
pub struct HeadlessWindowCapability;

pub use HeadlessWindowCapability as WindowCapability;

#[cfg(feature = "desktop")]
#[actor(singleton, runtime::desktop)]
pub struct DesktopWindowCapability;

#[actor(singleton, runtime::synthetic)]
pub struct SyntheticWindowCapability;
```

The concrete headless, desktop, and synthetic window identities all claim one
crate-owned namespace constant. `WindowCapability` remains the neutral alias
that consumers name through `ctx.actor::<WindowCapability>()`; runtime
variants must not repeat a namespace literal in their declarations or leak
platform identity into callers. The headless implementation lives in the
default `runtime/mod.rs`; keyed alternatives live in `runtime/desktop/` and
`runtime/synthetic.rs`.

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

Caller-facing `aether.<cap>.*` kinds belong in the crate's own `src/kinds.rs`
and are re-exported from its crate root, `src/lib.rs`. The marker face remains
wasm-safe: schema, serde, and lightweight data dependencies only.

Keep a kind in `aether-kinds` only when a named upstream consumer cannot depend
on the cap's own crate, or when it is truly substrate-wide. Document that
must-stay reason next to the exception. Current examples include
`KindDescriptorWire`, which `aether-inventory` and `aether-fleet` both carry,
and some engine/control/capture contracts consumed by MCP or substrate core.

Being consumed by MCP is not itself a must-stay reason. `aether-mcp` takes a
capability crate identity-only — `default-features = false`, no runtime, no
substrate — precisely so a cap can own its kinds and still have them reach the
harness's static `descriptors::all()` vocabulary through the link. `aether-fs`
and `aether-inventory` are both wired that way; reach for the same shape before
promoting a kind to the central crate.

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
- `feature = "runtime"` for substrate-typed state and handlers, including the
  cap's own heavy backend — `aether-audio` pulls cpal there, `aether-render`
  pulls wgpu;
- a further platform feature above `runtime` when only some chassis need it.
  `aether-render` and `aether-window` both carry `desktop = ["runtime",
  "dep:winit"]`, so headless, harness, and wasm consumers never build winit. An
  actor whose runtime impls should gate on such a feature instead of the generic
  `runtime` names it with `#[actor(runtime_feature = "…")]`;
- native-target gates for provider/subprocess code that has no useful wasm
  marker face.

A cap no wasm guest can address carries no ladder at all. `aether-rpc` (the TCP
transport) and `aether-fleet` (fork+exec and sockets) are native-only: there is
no marker build to preserve, so their dependencies are flat and unconditional and
neither declares a `runtime` feature. The exemption is narrow — it is the absence
of any guest that can name the identity, not the presence of a heavy backend, and
each crate's manifest states that reason where a reader meets the flat
dependency list.

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

### One actor, one config

An actor's `type Config` is a single `#[derive(aether_substrate::Config)]`
struct. `ConfigMember` admits no hand-written impls, so a composite that nests
two config structs to carry both their knobs does not compile — that is the
design holding, not an obstacle to route around. When a cap needs another knob,
the sanctioned moves are: add a field to the existing config, pinning
`#[config(env = "…")]` when the struct's `env_prefix` reads wrong for it; pass it
as the `params` argument of `with_actor_configured` when it is composer-computed
construction input rather than an operator-resolvable knob; or give it a cap of
its own.

A crate may still carry a second derive-`Config` when no actor receives it.
`aether-rpc` holds two: `RpcServerConfig` is `RpcServerCapability`'s
`type Config`, while `FrameSizeConfig` resolves off the same source stack and is
lowered into `aether-codec`'s frame-size ceiling at boot
(`aether_codec::frame::install_max_frame_size`), reaching no actor's `init`. The
one-config rule binds `type Config`, not the file count in a crate.

### Where the config struct lives

Three placements are in use. They are one rule — the struct sits at the tier that
owns it:

| Placement | When | Current |
|---|---|---|
| crate-root `config.rs` | the crate's one config, visible to the marker face too | `fs/`, `process/` |
| `runtime/config.rs` | only the runtime half ever names it | `audio/`, `render/`, `lifecycle/` |
| beside its actor in a cluster subdirectory | the crate hosts several actors | `http/client/`, `http/server/`, `rpc/server/`, `fleet/server/` |

The `#[cfg_attr(feature = "runtime", …)]` gate follows the placement, not the
crate. A file that also compiles on the wasm marker build gates each derive and
each `#[config(…)]` attribute itself; a file reached only through
`#[cfg(feature = "runtime")] mod runtime;` is already gated once at the module,
and repeating the gate per item is noise that reads as a rule someone forgot
elsewhere. That is the whole difference between `fs` / `http` / `process`
carrying `cfg_attr` and `audio` / `render` / `lifecycle` deriving bare. `rpc` and
`fleet` carry neither, having no marker build to preserve — see
[Feature ladder](#feature-ladder).

### Chassis-owned boot knobs are not cap config

A knob named after a capability does not thereby belong to that capability's
crate. `WindowConfig` lives in `aether-chassis`, not `aether-window`, because no
actor in `aether-window` receives it — every window runtime declares
`type Config = ()`. It is the desktop chassis's own boot knob group (window mode
and title, application name, wireframe), named by the fleet-wide config registry
in `boot` and the shared CLI roots in `cli`, and lowered by the winit driver and
the render surface. Moving it into the cap crate would hand that crate a config
nothing in it reads.

`aether-fs`'s `NamespaceRoots` is the counter-case decided by the same test: the
shared CLI roots name it too, but `FsCapability` is what receives it at `init`,
so it stays in the cap crate. Ask which crate's code receives the resolved value,
not which crate's name appears in the knob.

## Fail-fast unsupported actors

If a public mailbox exists conceptually but a chassis cannot provide the
resource, prefer an explicit unsupported actor that replies with the ordinary
error shape. Examples include headless render/window/clipboard companions and
the substrate-harness unsupported marker.

Most companions mirror the primary cap's identity/runtime split symmetrically:
their identity ZST lives in the crate-root `headless` module
(`src/headless.rs`, always-on, declared with
`#[actor(singleton, runtime::headless)]`) and their runtime half in
`src/runtime/headless.rs`, a nested child covered by the same `mod runtime;`
gate as the primary runtime. The module-path argument tells the struct-hosted
`#[actor]` harvest which file to read, resolved relative to the invoking file.
`aether-render` and `aether-clipboard` are exemplars.

Window deliberately uses the concrete `HeadlessWindowCapability` as the
fail-fast default runtime, with `WindowCapability` retained as its neutral
consumer alias. Desktop and `runtime::synthetic` implementation types claim
the same shared namespace. That shape is appropriate when callers must remain
platform-neutral and multiple runtimes are mutually exclusive chassis choices.

Do not create a stub for fire-and-forget traffic unless it produces useful
diagnostics and avoids misleading success. The goal is bounded failure, not
pretend capability.

## Tests by boundary

| Contract | Test level |
|---|---|
| kind/schema/validation | unit tests beside `kinds.rs`/validator |
| state machine/adapter mapping | runtime unit tests with a fake adapter |
| actor mail/reply/settlement | focused SubstrateHarness test |
| marker/runtime feature split | `cargo build -p <cap> --no-default-features` |
| chassis composition/resource | chassis/SubstrateHarness integration test |
| hub/process routing | FleetHarness |

Shared RPC test echo code currently lives under
`aether-rpc/src/server/test_echo.rs`; engine/proxy tests reuse it.
Do not copy old paths such as a crate-root `test_echo.rs` or `test_chassis.rs`.

## Review checklist

- Identity and kinds are usable without native runtime dependencies.
- Capability-owned kinds did not leak into `aether-kinds` without a reason.
- Runtime state has one actor owner; sidecars return events/results.
- Every request that promises a reply resolves on success, error, disablement,
  and shutdown.
- Every reply `Err` arm names its payload `error`. The field is wire-visible —
  a component and `describe_kinds` both read it — so one spelling holds across
  every cap. `reason` is reserved for an arm that is not a failure, the way
  `ProgramTimingsResult::Absent { reason }` reports a device that cannot
  measure.
- A `*_result` kind without an `Err` arm says in one line why it cannot fail.
  Reading a process-global link-time table and enumerating a live registry are
  both infallible; a lookup against that same registry is not, and carries the
  `Err` arm.
- Queues, bytes, timeouts, retries, and callback work are bounded.
- Every intended chassis installs the real or explicit unsupported actor.
- Resolved config appears in the appropriate `--print-config` surface.
- Live handler/kind inventory sees the contract.
- The selected test crosses the changed boundary.
- A load-bearing new trust/compatibility decision has an ADR.

## Current exemplars

- Light single-file runtime: `fs/`, `clipboard/`
- Heavy runtime directory: `audio/`, `render/`, `component/`, `lifecycle/`
- Neutral identity with desktop/headless/synthetic runtimes: `window/`
- Multi-actor cluster: `fleet/`, `http/`, `tcp/`
- Self-contained guest provider components: `anthropic/`
- Split test support: `rpc/server/test_echo.rs`

See [Guest/native boundaries](architecture/guest-native-boundary.md),
[Configuration](systems/configuration.md), and the
[capability index](reference/capability-index.md).
