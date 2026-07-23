# ADR-0164: Window actor owns native window integration

- **Status:** Accepted (shipped — the window-owned application, multi-window vocabulary, keyed render targets, consumer migration, desktop delegation, and `aether-input` retirement are on `main`)
- **Date:** 2026-07-23

## Context

ADR-0160 moved the three `aether.window` control handlers onto a pumped
`DesktopWindowCapability`, but it deliberately left winit's
`ApplicationHandler` in `aether-chassis-desktop`. That intermediate boundary
has become the wrong permanent one.

The capability currently owns only `set_mode`, `set_title`, and `focus`.
The chassis driver still owns the behavior that makes a window a window:

- creation in `resumed`, the `ApplicationHandler` implementation, and the
  complete `winit::event::WindowEvent` match;
- cursor position, IME composition, occlusion, redraw, close, and lifecycle
  policy;
- translation from winit events into Aether input kinds;
- the single `Option<Arc<Window>>` and one-shot `WindowCell` shared with the
  window and render actors;
- an `aether.input` mailbox hop plus one cached `KindId` field for every
  published input kind.

This produces three problems.

First, ownership is split by mechanism rather than domain. The window actor can
mutate a handle, while the chassis decides what every native window event
means. Adding a winit feature therefore requires editing the chassis even when
it has no chassis concern.

Second, the shape is intrinsically single-window. The `WindowCell` can be
filled once, control requests carry no window identity, every input
subscription is process-global, input events carry no source window, and the
render actor owns one surface. Extending that collection of singletons would
create parallel maps in the chassis, window actor, input actor, and render
actor with no single owner of their lifecycle.

Third, the input publication path has retained exactly the synchronization
burden ADR-0068 removed from subscriptions. Although `Kind::ID` is a
compile-time schema hash under ADR-0030, desktop boot resolves and stores a
manual list of input kind ids. Each winit event is then encoded in the chassis,
mailed once to `aether.input`, decoded by `InputCapability`, and encoded again
for its subscribers. The list is redundant, the hop carries no domain
meaning, and neither has a sensible multi-window extension.

The threading constraint is real but does not justify the ownership split.
The desktop driver must continue to run winit's one `EventLoop` on the
application thread. `PumpedSlot` is deliberately `!Send`, and both window and
render handlers already execute on that same thread. Worker-pool actors must
remain off the winit thread, native `Arc<Window>` handles must not become wire
payloads, and settlement waits must continue pumping render so a frame cannot
deadlock.

The desired boundary is therefore:

- the chassis owns the application thread, boot order, and process lifetime;
- `aether-window` owns native window behavior, including the winit
  `ApplicationHandler`;
- one application-scoped window actor owns every window and the routing of
  events originating from those windows;
- `aether-render` owns every GPU surface, keyed by the same engine window
  identity;
- the chassis only composes and runs those pieces.

## Decision

Make `aether.window` an application-scoped multi-window manager and move the
desktop winit application into `aether-window`. Retire the desktop
`aether.input` indirection, replace `WindowCell` with explicit same-thread host
effects, and key window and render behavior by a stable engine `WindowId`.

### 1. One manager actor, with engine-owned window identities

There is one pumped window actor per application, not one actor per native
window. Winit exposes one `ApplicationHandler` over one `EventLoop`; the
manager actor mirrors that ownership and keeps cross-window policy in one
place.

`WindowId` is an Aether-generated `u64` newtype. It is distinct from
`winit::window::WindowId`, which is platform-owned, opaque, and unsuitable as a
wire contract. The desktop state owns both directions of the mapping:

```rust
pub struct DesktopWindowCapabilityState {
    next_window_id: u64,
    windows: HashMap<WindowId, DesktopWindowState>,
    winit_windows: HashMap<winit::window::WindowId, WindowId>,
    subscribers: WindowSubscribers,
    pending_host_actions: VecDeque<WindowHostAction>,
}

struct DesktopWindowState {
    window: Arc<winit::window::Window>,
    mode: WindowMode,
    title: String,
    cursor: (f32, f32),
    composing: bool,
    modifiers: Modifiers,
    focused: bool,
    occluded: bool,
    closing: bool,
}
```

Public control mail becomes explicitly window-addressed. The window crate owns
the request/reply kinds and a sender facade shaped approximately as:

```rust
pub struct WindowId(pub u64);

pub enum WindowSelector {
    One(WindowId),
    All,
}

pub struct WindowSpec {
    pub title: String,
    pub mode: WindowMode,
    pub size: Option<WindowSizeRequest>,
}

pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub mode: WindowMode,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    pub occluded: bool,
}

pub trait WindowMailboxExt {
    fn list(&self);
    fn create(&self, spec: WindowSpec);
    fn close(&self, window: WindowId);
    fn set_title(&self, window: WindowId, title: String);
    fn set_mode(&self, window: WindowId, mode: WindowMode);
    fn focus(&self, window: WindowId);
    fn request_redraw(&self, window: WindowId);

    fn subscribe<K: Kind>(&self, selector: WindowSelector);
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector);
}
```

`CreateWindow` replies with `WindowInfo`; `ListWindows` returns every live
window in `WindowId` order. `WindowSelector::All` includes windows created
after the subscription. There is no implicit "primary", "focused", or
"current" target. The chassis's boot window is simply the first
`WindowSpec`, realized during `resumed`.

Introduce a neutral, addressing-only `WindowCapability` identity for
`ctx.actor::<WindowCapability>()`. Desktop and headless runtime identities may
remain distinct implementation types, but consumers no longer name a
platform runtime to address `aether.window`. This completes the neutral
identity follow-up recorded by ADR-0160.

### 2. `aether-window` owns the winit application

`aether-window` exposes a `DesktopWindowApplication<I>` that implements
`ApplicationHandler<DesktopWindowUserEvent>`. It owns the pumped window slot
and a narrow integration supplied by the chassis:

```rust
pub trait DesktopWindowIntegration {
    fn attach_window(&mut self, id: WindowId, window: Arc<Window>) -> Result<(), String>;
    fn detach_window(&mut self, id: WindowId);
    fn windows_dirty(&mut self, windows: &[WindowId]);
    fn request_shutdown(&mut self);
    fn pump_while_settling(&mut self, settlement: MailId) -> WaitOutcome;
}

pub struct DesktopWindowApplication<I> {
    window_slot: PumpedSlot<DesktopWindowCapability>,
    integration: I,
}

impl<I: DesktopWindowIntegration> ApplicationHandler<DesktopWindowUserEvent>
    for DesktopWindowApplication<I>
{
    // resumed, window_event, user_event, and about_to_wait live here.
}
```

The integration is deliberately semantic. It does not receive raw winit
events, maintain cursor or IME state, translate input kinds, or decide window
lifecycle. Its desktop implementation connects the already-pumped render and
lifecycle actors and process shutdown.

The chassis driver collapses to composition and thread ownership:

```rust
let (window_slot, window_wake) =
    ctx.boot_pumped_actor::<DesktopWindowCapability>((), window_params)?;
let (render_slot, render_wake) =
    ctx.boot_pumped_actor::<RenderCapability>(render_config, render_params)?;

let integration = DesktopRenderIntegration::new(
    render_slot,
    lifecycle_route,
    shutdown,
);
let mut application =
    DesktopWindowApplication::new(window_slot, integration, initial_window);

application.install_wakes(event_loop.create_proxy(), window_wake, render_wake);
event_loop.run_app(&mut application)?;
```

`aether-chassis-desktop` continues to own the call to `run_app`, and therefore
the application thread. Moving the `ApplicationHandler` implementation does
not create a thread or transfer thread ownership to the actor.

### 3. Pumped actors gain explicit host ingress

A winit callback is already executing on the pumped actor's owning thread, but
it is not an inbound Aether envelope. Encoding every winit callback as an
internal mail would invent wire kinds for a host-only API, lose access to
`ActiveEventLoop`, and add another queue between two objects on the same
thread.

Add a bounded host-ingress operation to `PumpedSlot`:

```rust
pub fn host_turn<R>(
    &mut self,
    turn: impl FnOnce(&mut A::State, &mut NativeCtx<'_>) -> R,
) -> Option<R>;
```

`host_turn`:

- is callable only through the `!Send` slot on its owning thread;
- stamps actor-local log, trace, and context state exactly like dispatch;
- creates a send-capable `NativeCtx` whose outbound mail starts fresh
  external-event roots;
- flushes outbound work through the normal mailer after the closure returns;
- cannot let a state or context reference escape the closure;
- is non-reentrant and does not drain either pumped slot recursively.

It is host ingress, not a general replacement for handlers. Actor-to-actor
commands still arrive as mail and are processed by `drain_available`.
Read-only `read_state` remains for bounded loop decisions.

Every winit callback follows one order:

```rust
self.window_slot.drain_available();

let effects = self.window_slot.host_turn(|state, ctx| {
    state.window_event(event_loop, winit_id, event, ctx)
});

self.apply_window_effects(effects);
self.pump_render();
```

Control handlers that need `ActiveEventLoop`, especially `CreateWindow`, queue
a `WindowHostAction` while retaining their reply. The application takes those
actions in a host turn, realizes them with the callback's
`ActiveEventLoop`, then completes the actor transition and reply in a second
host turn. No `ActiveEventLoop` reference is stored in actor state.

Mail emitted by a host turn is enqueued through the scheduler. A component or
inline subscriber handler never executes inside the winit callback. A
thread-identity test makes that property load-bearing.

### 4. Window-originated input is published by the window actor

The `aether.input` actor and its extra desktop mail hop retire. Its useful
state—the subscriber table, mailbox validation, and monitor-driven cleanup—is
absorbed by `aether.window`.

Subscriptions remain generic over `KindId`, now with a selector:

```rust
struct WindowSubscribers {
    all: HashMap<KindId, BTreeSet<MailboxId>>,
    specific: HashMap<(WindowId, KindId), BTreeSet<MailboxId>>,
    monitors: HashMap<MailboxId, MonitorHandle>,
}

fn publish<K: Kind>(
    &self,
    ctx: &mut NativeCtx<'_>,
    window: WindowId,
    event: &K,
) {
    ctx.fanout(self.subscribers.recipients(window, K::ID), event);
}
```

`recipients` unions and deduplicates the `All` and `One(window)` sets.
Subscribe, unsubscribe, reflexive subscribe, explicit-mailbox subscribe, bulk
unsubscribe, validation, and monitor cleanup preserve the current
`aether.input` semantics.

Every window-originated event carries its source `WindowId`. This is a
pre-1.0 wire break for `Key`, `KeyRelease`, `MouseButton`,
`MouseButtonRelease`, `MouseWheel`, `MouseMove`, `WindowSize`, `TextInput`,
`ImePreedit`, and `Modifiers`; their schema-hashed ids change as intended by
ADR-0030. Add `WindowOpened` and `WindowClosed` lifecycle events.

There is no central event-kind declaration, registry lookup, whitelist, or
cached id table. A winit match arm constructs a typed event and calls
`publish`; the generic path takes `K::ID`. Adding another native window event
means defining its kind and publishing it at the mapping site.

This supersedes ADR-0021 and ADR-0068 only for window-originated input. A
future gamepad, raw HID, or network controller is a distinct source actor; it
does not recreate a generic input middleman between a source actor and its
subscribers.

### 5. Render owns a surface per `WindowId`

Native handles cross from the window actor to render through owned,
same-thread host effects, never through wire mail or a shared mutable
registry:

```rust
enum WindowHostEffect {
    Created { id: WindowId, window: Arc<Window> },
    Closing { id: WindowId },
    Dirty { id: WindowId },
    Occluded { id: WindowId, occluded: bool },
    LastWindowClosed,
}

struct RenderCapabilityState {
    targets: HashMap<WindowId, RenderTarget>,
    // existing frame accumulators and capture state
}

struct RenderTarget {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    occluded: bool,
}
```

After a window host turn returns `Created`, the application calls render host
ingress to build and insert its surface. Only after a successful attach does
the retained `CreateWindow` request receive `Ok`. Attach failure removes the
new window and returns `Err`.

Close is two-phase: mark the window closing and stop publishing new work;
detach render, failing any capture for that target and dropping its surface;
then remove the final `Arc<Window>` from window state and publish
`WindowClosed`. Closing the last window requests normal global shutdown.

Surface-facing render control becomes window-addressed. Occlusion, resize,
capture, and redraw carry `WindowId`; a frame request carries the set of dirty
window ids. Multiple `RedrawRequested` callbacks coalesce into one global
`LifecycleAdvance`, after which render presents the committed scene to every
dirty, non-occluded target. The first multi-window contract keeps the existing
draw accumulator global—dirty windows display the same committed scene.
Independent per-window scene graphs are a separate render-vocabulary decision,
not hidden inside native window ownership.

Window and render are both pumped on the application thread, but neither
blocks waiting for a synchronous reply from the other. The application applies
owned host effects after each actor borrow ends. The settlement wait uses a
unified pump that can drain both window and render slots, so cross-mail cannot
deadlock a lifecycle chain.

ADR-0160's one-shot `WindowCell` decision and ADR-0161's single-surface
`WindowCell` boot path are superseded by this explicit attach/detach protocol.

### 6. Implementation slices

The implementation landed as independently green slices after this ADR:

1. **Host ingress:** add `PumpedSlot::host_turn`, external-root semantics,
   non-reentrancy, outbound-send, and thread-identity tests.
2. **Window vocabulary:** add `WindowId`, selectors, lifecycle and
   window-addressed control kinds, the neutral `WindowCapability`, and
   `WindowMailboxExt`; add `WindowId` to window event payloads.
3. **Window manager:** move subscription ownership and winit mapping into the
   desktop window actor; add `DesktopWindowApplication`; keep a compatibility
   bridge until the chassis swap.
4. **Multi-target render:** replace `WindowCell` and the single surface with
   explicit attach/detach and a `WindowId -> RenderTarget` map; target capture,
   resize, occlusion, and frames.
5. **Desktop swap and deletion:** reduce the chassis to composition, remove its
   `ApplicationHandler`, input-event state and mapping, cached kind ids,
   `WindowCell`, and desktop `InputCapability`; update headless companions,
   harnesses, examples, and guides.

The ADR remained Proposed until every slice landed, following ADR-0160 and
ADR-0161 convention. The complete chain is now on `main`, so this record is
Accepted.

## Consequences

- Multi-window identity, control, input routing, and native lifecycle have one
  owner. There is no chassis-side shadow registry to keep synchronized.
- The chassis still owns the application thread and process lifetime, while
  the window crate owns all winit behavior. The threading boundary is explicit
  instead of implied by file location.
- The boot-time kind lookup block and every per-kind `App` field disappear.
  Publication uses typed `K::ID`, and the encode/decode/re-encode
  `aether.input` hop disappears.
- `All` and `One(WindowId)` subscriptions support overlays, tools, and
  per-window controllers without an implicit focus policy. `All` including
  future windows avoids a resubscription race during creation.
- `PumpedSlot::host_turn` is new framework surface. It creates a deliberate
  second ingress path—external host events alongside envelopes—and therefore
  requires the same tracing, actor-local stamping, root lineage, and outbound
  guarantees as dispatch.
- The public input and window wire shapes break. Schema-hashed kind ids make
  stale producers fail loudly, but all components, examples, scenarios, MCP
  tooling, and documentation must migrate atomically.
- `aether-input` no longer exists. A future gamepad, raw HID, or network
  controller belongs to its concrete source actor and must not recreate a
  generic relay between source actors and subscribers.
- `aether-render` carries one surface/configuration bundle per live window and
  must define failure behavior per target. GPU memory and present work scale
  with window count.
- The first multi-window render contract shares one committed scene across
  targets. Distinct scenes or cameras per window require an explicit later
  render API rather than an accidental convention.
- Subscriber enumeration occurs on the application thread, but subscriber
  handlers execute through normal scheduler dispatch. High-frequency motion
  input remains one encode plus queued fan-out; it cannot run guest work
  inline on winit.
- Tests must cover two-window specific routing, `All` subscriptions across
  later creation, duplicate-recipient deduplication, closing one window
  without global shutdown, last-window shutdown, create/attach rollback,
  per-target occlusion and capture, monitor cleanup, and proof that subscriber
  handlers run off the winit thread.

## Alternatives considered

- **One pumped actor per native window.** Rejected: winit has one application
  handler and pumped actors are currently singleton claims. Per-window actors
  would still need a manager for creation, native-id routing, global
  subscriptions, and last-window policy, producing more indirection without
  removing the manager.
- **Keep `InputCapability` between the window actor and subscribers.**
  Rejected: it preserves an encode/decode/re-encode hop and a process-global
  subscription table exactly where source-window selection belongs.
- **Leave `ApplicationHandler` in the chassis and call helper methods in
  `aether-window`.** Rejected: the exhaustive winit match, callback ordering,
  and window-local state would still be chassis-owned; every new native event
  would continue crossing the boundary.
- **Let the window actor own or spawn the application thread.** Rejected: the
  chassis driver remains responsible for blocking process execution and
  platform startup. Winit's thread constraint is satisfied by pumping the
  actor on that thread; a second thread changes nothing and is invalid on
  platforms that require the main thread.
- **Share a mutable multi-window registry with chassis and render.** Rejected:
  replacing one `WindowCell` with an `Arc<Mutex<HashMap<...>>>` recreates the
  cross-owner seam ADR-0160/0161 removed. Explicit owned effects make attach
  and detach ordering visible.
- **Mail raw winit events to the actor.** Rejected: winit event and
  `ActiveEventLoop` types are host-only, not stable wire vocabulary, and the
  extra queue cannot express synchronous platform operations such as window
  creation. `host_turn` is the honest host boundary.
- **Use `winit::window::WindowId` as the public identity.** Rejected: it is an
  opaque platform token with no Aether wire or replay stability.
- **Synchronously request/reply between the pumped window and render actors.**
  Rejected: both require the same thread to make progress, so blocking either
  on the other deadlocks. The application applies owned effects after actor
  borrows end.
- **Keep a static list of input kind ids in the manager.** Rejected:
  ADR-0030 already gives every typed kind a schema-hashed `K::ID`; a second
  registry is redundant and can drift.
