# ADR-0167: Window manager supervises addressable window actors

- **Status:** Proposed
- **Date:** 2026-07-28
- **Amends:** ADR-0164

## Context

ADR-0164 made `aether.window` the application-scoped owner of winit, native
window state, selector-aware input publication, and same-thread render
attachment. That boundary has shipped and remains correct: the chassis owns
the application thread and process lifetime, while the pumped window manager
owns the one winit `ApplicationHandler` and every native `Arc<Window>`.

ADR-0164 deliberately stopped at one actor managing a map of windows. Public
control mail therefore still targets the manager and repeats a `WindowId` in
every payload:

```rust
ctx.actor::<WindowCapability>()
    .set_title(window_id, "Inspector");
```

This is an awkward endpoint for multi-window composition. A named window
cannot be handed to another subsystem as a typed actor address, an external
client cannot target it through the abbreviated address syntax added by
ADR-0166, and the manager has to decode both the operation and a second
identity inside the operation. Mutable presentation state such as the title
is also being asked to stand in for the stable name by which code wants to
find a window.

ADR-0166 supplied the missing identity mechanism. An instanced actor may
declare `ChildOf<WindowCapability>`, Rust callers may resolve it from the
manager mailbox, and string-addressed boundaries may expand:

```text
aether.window://main
    ->
aether.window/aether.window.instance:main
```

Using that mechanism does not mean moving winit onto a worker. There is one
winit event loop and one application callback object. `PumpedSlot` is `!Send`,
and the render integration must remain on that same application thread. A
per-window actor can therefore be an addressable, supervised control endpoint,
but it cannot own the native handle or become a second winit application.

The design must also preserve the properties already shipped by ADR-0164:

- the chassis only composes and runs the window application;
- the pumped manager is the bespoke home of all winit translation and native
  lifecycle policy;
- render owns a keyed target per engine window identity and receives native
  handles only through same-thread host effects;
- subscriptions may select one window or all current and future windows;
- closing through the platform callback must not wait for a worker-pool round
  trip;
- desktop, synthetic, and headless runtimes share logical actor identities and
  namespace constants rather than copied strings;
- native handles and winit types never enter wire mail.

## Decision

Keep `aether.window` as the single pumped manager and add one ordinary pooled,
instanced child actor for each live window. The manager supervises those child
actors. A child is the public control address for one window; the manager
remains the only owner of native state, global enumeration, creation,
subscriptions, and cross-window policy.

### 1. The public actor graph has one manager and named window children

Add a neutral `WindowInstance` identity and desktop and synthetic runtime
variants. All variants read their namespace from one crate-owned constant and
declare the same logical placement permission:

```rust
const WINDOW_NAMESPACE: &str = "aether.window";
const WINDOW_INSTANCE_NAMESPACE: &str = "aether.window.instance";

#[actor(singleton, root)]
pub struct HeadlessWindowCapability;
pub use HeadlessWindowCapability as WindowCapability;

#[actor(instanced, child_of(WindowCapability))]
pub struct HeadlessWindowInstance;
pub use HeadlessWindowInstance as WindowInstance;

#[cfg(feature = "desktop")]
#[actor(
    instanced,
    child_of(WindowCapability),
    runtime::desktop,
)]
pub struct DesktopWindowInstance;

#[cfg(feature = "synthetic")]
#[actor(
    instanced,
    child_of(WindowCapability),
    runtime::synthetic,
)]
pub struct SyntheticWindowInstance;

#[runtime]
impl NativeActor for DesktopWindowInstance {
    const NAMESPACE: &'static str = WINDOW_INSTANCE_NAMESPACE;
    // ...
}
```

The existing manager runtime variants continue to use `WINDOW_NAMESPACE`.
ADR-0166 compares the logical parent identity at runtime, so a desktop or
synthetic manager may spawn its matching child with the neutral
`WindowCapability` parent permission without inventing a platform-specific
public lineage.

The child instance name is immutable and is supplied separately from the
mutable title:

```rust
pub struct WindowSpec {
    pub name: String,
    pub title: String,
    pub mode: WindowMode,
    pub size: Option<WindowSizeRequest>,
}

pub struct WindowInfo {
    pub id: WindowId,
    pub name: String,
    pub title: String,
    // mode, size, focus, and occlusion
}
```

`name` must be a valid actor instance segment. It is unique among pending and
live windows beneath the manager. Native instanced actors currently retire a
canonical name when they shut down, so a closed window name cannot be reused
during the same chassis lifetime. That behavior is explicit rather than
silently weakening actor tombstones; reusable named slots would require a
separate actor-lifecycle decision.

Rust callers resolve through the actor identities:

```rust
let manager = ctx.actor::<WindowCapability>();
let main = manager.resolve::<WindowInstance>("main");
let palette = manager.resolve::<WindowInstance>("palette");

main.set_title("Game");
palette.focus();
```

String-addressed boundaries use the shared ADR-0166 resolver. No window alias
table or hard-coded shortened namespace is added:

```text
aether.window://main
aether.window://palette
```

The expanded canonical paths remain authoritative. Branching and multiple
allowed parents do not create a diamond for window identity: each canonical
path includes its complete parent lineage, and ADR-0166 rejects an abbreviated
step if more than one logical child namespace is possible.

### 2. `WindowId` is the child actor's mailbox identity

Remove the manager's second monotonic identity allocator. The engine
`WindowId` remains a wire-safe `u64` newtype, but its value is the raw
`MailboxId` of the named child:

```rust
let child = ctx
    .actor::<WindowCapability>()
    .resolve::<WindowInstance>(&spec.name);
let id = WindowId(child.mailbox_id().0);
```

The same value keys manager state, winit-to-engine lookup, render targets,
input events, selectors, and public `WindowInfo`. Conversions between
`WindowId` and `MailboxId` are explicit and lossless. There is no lookup table
from a pretty window id to an actor id and no opportunity for those identities
to disagree.

The instance name and canonical actor address are stable; the title, size,
mode, focus, and occlusion remain mutable properties. `ListWindows` stays
deterministic by returning live entries in ascending `WindowId` order.

### 3. The manager and child expose different mail surfaces

Split the sender facade by responsibility:

```rust
pub trait WindowManagerMailboxExt {
    fn list(&self);
    fn create(&self, spec: WindowSpec);

    fn subscribe<K: Kind>(&self, selector: WindowSelector);
    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId);
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector);
    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId);
    fn unsubscribe_all(&self, mailbox: MailboxId);
}

pub trait WindowMailboxExt {
    fn close(&self);
    fn set_mode(&self, mode: WindowMode, width: Option<u32>, height: Option<u32>);
    fn set_title(&self, title: &str);
    fn focus(&self);
    fn request_redraw(&self);
}
```

Manager requests continue to target `WindowCapability`. Per-window control
requests target `WindowInstance` and no longer contain a `window` field:

```rust
pub struct CloseWindow;
pub struct SetWindowTitle {
    pub title: String,
}
pub struct FocusWindow;

pub enum CloseWindowResult {
    Ok,
    Err { error: String },
}
```

Mode and redraw follow the same shape, and control replies no longer echo the
target already named by the recipient mailbox. This is a pre-1.0 wire break;
the schema-hashed kind ids change as intended.

`WindowSelector::One(WindowId)` remains on the manager subscription surface.
Input events retain `WindowId` because an `All` subscriber must know which
window produced an event. Global subscriptions are manager policy rather than
duplicated per child.

### 4. Children forward control; the pumped manager performs it

A per-window child owns no `Arc<Window>`, winit state, render target, or global
subscriber table. Its state is only the small forwarding state machine needed
to preserve request/reply settlement while work crosses to the manager.

Conceptually, each id-less public command becomes one private id-bearing
manager command:

```rust
enum WindowCommand {
    Close,
    SetMode { mode: WindowMode, width: Option<u32>, height: Option<u32> },
    SetTitle { title: String },
    Focus,
    RequestRedraw,
}

struct ApplyWindowCommand {
    window: WindowId,
    command: WindowCommand,
}

struct ForwardContext {
    pending: u64,
}
```

The child derives the target from `ctx.self_id()`, retains the original
`InboundMail`, and uses the normal typed context adapter for correlation:

```rust
let pending = state.retain(ctx.take_inbound());
ctx.actor::<WindowCapability>()
    .with_context(&ForwardContext { pending })
    .apply_window_command(ApplyWindowCommand {
        window: WindowId(ctx.self_id().0),
        command,
    });
```

When the manager replies, the child calls
`ctx.take_context::<ForwardContext>()`, removes the matching retained inbound,
and sends the public result through that guard. Concurrent requests remain
independently correlated. Missing or duplicate context is an invariant error,
not a guessed reply target.

The private manager command and result kinds are crate implementation details.
They do not make id-bearing control public again. The manager validates that
the target is live before touching host state.

This forwarding is asynchronous. The child never blocks its worker waiting
for the pumped manager, and the manager never blocks the application thread
waiting for the child. Mail to the manager triggers the existing winit event
loop wake; manager replies return through the ordinary scheduler.

### 5. Creation is published only after the complete graph is live

Creation is a transaction across the manager's native state, render target,
and supervised child. For a requested name the manager:

```text
validate and reserve name + predicted child mailbox
    -> create native window on ActiveEventLoop
    -> attach render target under WindowId(child_mailbox)
    -> spawn matching pooled child under aether.window
    -> monitor the child
    -> mark live, publish WindowOpened, reply CreateWindowResult::Ok
```

The child is spawned only after native creation and render attachment succeed,
so a public child address never represents a successfully opened window with
missing host resources. Spawn or monitor failure detaches render, drops the
manager's native handle and mappings, clears the reservation, and replies with
`CreateWindowResult::Err`. No `WindowOpened` event is published on a failed
transaction.

The boot window uses the same transaction and receives an explicit configured
name (normally `main`). Initial-window failure preserves ADR-0164's normal
shutdown behavior. Desktop and synthetic runtimes construct the same actor
graph; the headless manager reports that no window peripheral is available and
spawns no child.

`NativeCtx::spawn_child(...).finish()` is currently authoritative when it
returns. If the pending ADR-0165 handler-spawn work changes that contract, this
transaction must wait for the authoritative activation result before
publishing success; it must not infer liveness from a predicted mailbox id or
from enqueueing a spawn request.

### 6. The manager supervises child lifetime and owns cleanup

The manager stores a `MonitorHandle` for each live child. A child monitor
notice is authoritative evidence that its address/control endpoint departed.
If that departure was unexpected, the manager drives the same close state
machine used by normal control:

```text
MonitorNotice(child)
    -> mark window closing
    -> detach render target
    -> remove native and winit mappings
    -> drop final manager Arc<Window>
    -> publish WindowClosed
    -> request process shutdown only if it was the last live window
```

Normal programmatic close completes the manager's host transition, replies to
the child, and then the child calls `ctx.shutdown()`. A native close request
from winit enters the pumped manager directly, performs native/render cleanup,
and sends a private retire message to the child. It does not detour through
the worker pool before closing the native window. The eventual monitor notice
is idempotent cleanup, not a second close.

Manager shutdown fails retained manager and child requests before guards are
dropped. Chassis teardown remains the final backstop for any child still live
when the root manager unwires.

Subscriber monitoring remains separate state. The prerequisite subscription
cleanup removes registry ownership and mailbox validation from
`WindowSubscribers`; the actor handler validates explicit targets, installs
monitors, and purges subscriptions on notices. Child supervision uses the same
actor-boundary principle rather than teaching a data container how to query
global liveness.

### 7. Thread and render ownership do not move

The runtime topology is:

```text
application thread
  DesktopWindowApplication
    -> pumped aether.window manager
         owns winit mapping + Arc<Window> + host actions
    -> pumped aether.render
         owns WindowId -> RenderTarget

worker pool
  aether.window/aether.window.instance:main
  aether.window/aether.window.instance:palette
    -> address and forward control only
```

Winit callbacks continue to drain the pumped manager, enter `host_turn`, apply
owned effects to render after the manager borrow ends, and pump render. Neither
per-window child nor an SDK caller can access `ActiveEventLoop`, `Window`, or a
GPU surface. Render continues to interact with the manager only through the
existing same-thread attach/detach/dirty/occluded effects keyed by `WindowId`;
it does not mail or monitor window children.

## Consequences

### Positive

- Every live window is a typed actor recipient. Rust uses
  `resolve::<WindowInstance>(name)` and MCP/configuration may use
  `aether.window://name`; both produce the canonical child mailbox.
- The recipient address selects the window, so public control payloads and
  replies stop duplicating `WindowId`.
- Actor identity, window identity, render key, input source, and selector key
  become one value rather than synchronized ids.
- The pumped manager remains the single coherent home for winit and native
  lifecycle policy. Per-window actors do not weaken the application-thread
  boundary.
- Monitoring gives the manager an explicit failure signal for each public
  endpoint and makes unexpected child departure clean up native and render
  resources.
- Desktop and synthetic tests exercise the same manager/child addressing
  shape, while headless stays fail-fast.

### Negative

- A control request adds two queued actor hops: caller to child, child to
  manager, then the reverse reply path. Platform-originated close bypasses
  those hops so native responsiveness is unchanged.
- Creation spans native, render, actor-spawn, and monitor state and therefore
  needs explicit rollback at every boundary.
- `WindowSpec`, `WindowInfo`, every control kind, and their schema hashes
  change before 1.0. Existing examples, harness helpers, MCP scenarios, and
  guides must migrate together.
- Native actor tombstones make a closed instance name unavailable until the
  chassis restarts. Applications needing reusable names must model a stable
  long-lived slot or wait for a separate lifecycle feature.
- Each window consumes a pooled actor slot and a monitor entry in addition to
  its existing native and render resources.

### Neutral and follow-on

- `ChildOf<WindowCapability>` remains a placement permission. Supervision is
  implemented by the manager's monitor, not implied by the marker trait.
- Input publication and selector-aware subscriptions stay manager-owned.
  `WindowId` remains present on input and lifecycle events even though control
  payloads become id-less.
- Independent per-window scenes, cameras, or render actors are not introduced.
  The renderer keeps ADR-0164's shared committed scene and keyed targets.
- This ADR supersedes ADR-0164's choice of no per-window actor, its monotonic
  manager-local id allocator, and its manager-addressed control payloads. It
  preserves ADR-0164's thread, native ownership, subscription, render, and
  chassis boundaries.
- Verification must cover typed/canonical/abbreviated identity equivalence;
  two-window control isolation; `All` and `One` subscription routing; duplicate,
  invalid, and retired names; create rollback; expected and unexpected child
  departure; closing one window without global shutdown; last-window shutdown;
  and proof that child handlers run off the winit thread.

## Alternatives considered

- **One pumped actor per native window.** Rejected: winit still has one
  `ApplicationHandler`, pumped slots are application-thread resources, and a
  manager is still required for creation, native-id routing, subscriptions,
  and last-window policy.
- **Let pooled children own `Arc<Window>`.** Rejected: it transfers native
  access across the application-thread boundary and tangles render attachment
  with worker scheduling.
- **Keep every control on the manager and only make prettier ids.** Rejected:
  it leaves windows without typed recipient addresses and duplicates identity
  in every command.
- **Keep a monotonic `WindowId` beside the child mailbox.** Rejected: it
  requires another bidirectional map and permits actor, input, and render
  identities to drift.
- **Spawn the child before native and render work.** Rejected: external callers
  could reach an address whose window never becomes usable, and rollback would
  expose a transient false-success endpoint.
- **Move subscriptions or render targets into each child.** Rejected: global
  `All` subscriptions, one winit callback, and one renderer would still need
  manager-owned aggregation while creating more cross-thread state.
- **Route winit close through the child first.** Rejected: platform lifecycle
  already arrives at the authoritative pumped owner and must not wait for a
  worker round trip.
- **Keep child actors alive after close so names can be reused.** Rejected for
  the first implementation: a dormant address would no longer mean a live
  window and would require a separate activation/vacate protocol. Native actor
  tombstone semantics remain honest.

## Related

- ADR-0099 — canonical actor identity and lineage-folded `MailboxId`.
- ADR-0119 — actor addressing through resolver strategies.
- ADR-0164 — window-owned winit integration and multi-window manager.
- ADR-0165 — handler-driven spawn semantics; creation must consume its
  authoritative activation result if that work changes native spawn.
- ADR-0166 — typed parent/child placement, mailbox resolution, and abbreviated
  external addresses.
