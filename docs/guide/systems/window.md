# Window

> **Governing ADR:** [ADR-0035](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0035-substrate-chassis-split.md)
> (the substrate/chassis split). The window surface is desktop-only — it lives
> behind the chassis that owns a real OS window — and the contract is **stable**.

An actor never touches the OS window directly. Like everything else it does, a
window change crosses the mail boundary: the desktop chassis owns the window, and
an actor reaches it by mailing a request to the `aether.window` mailbox and
handling the reply. Three operations — switch presentation mode, set the title,
bring the window to the front — and each one replies with the value actually
applied.

That reply is the part to hold onto. A window request is advisory: the OS can
adjust a mode, clamp a size, or decline to honor a focus call exactly as asked.
So the reply carries the resolved state, not an echo of what you sent — the reply
is the truth about what the window now is.

## Why it exists

Window management has to run on one specific thread — the OS event-loop thread
the desktop chassis already pumps — and only the desktop chassis has a window at
all. Routing it through mail keeps both facts behind the same boundary every
other subsystem uses: an actor mails `aether.window` exactly as it mails
`aether.render` or `aether.fs`, and the chassis is the one party that knows
whether a window exists and which thread may touch it. A chassis without a window
(headless, hub) registers the same mailbox but fails the request fast instead of
pretending.

The reply-with-applied-value contract exists because a window op can't promise to
do exactly what it's told. The window manager owns the final say on geometry and
focus, so the only honest answer is the state after the change landed. Returning
it on every reply means a caller learns the resolved mode or title in the same
exchange, with no follow-up query.

## What it does

**One mailbox, three operations.** Everything addresses the `aether.window`
mailbox. Each request kind pairs with a reply kind that names the same operation:

| Request | Fields | Reply | `Ok` carries |
|---|---|---|---|
| `aether.window.set_mode` | `mode`, `width?`, `height?` | `aether.window.set_mode_result` | `mode`, `width`, `height` |
| `aether.window.set_title` | `title` | `aether.window.set_title_result` | `title` |
| `aether.window.focus` | — (none) | `aether.window.focus_result` | — (`Ok` ack) |

Each reply is an `Ok` / `Err` enum. The `Ok` arm carries the resolved state; the
`Err` arm carries a reason string and means nothing changed.

**`set_mode` switches presentation mode.** `mode` is one of three shapes:

- **`Windowed`** — a normal window. The optional `width` / `height` request a
  size in physical pixels; they apply only in this mode, and fullscreen modes
  size themselves.
- **`FullscreenBorderless`** — borderless fullscreen on the current monitor,
  sized to it.
- **`FullscreenExclusive { width, height, refresh_mhz }`** — exclusive
  fullscreen at one specific video mode. The substrate matches the triple against
  the monitor's supported modes and replies `Err` if none matches exactly — it
  fails loud rather than silently falling back. The reply's `Ok` carries the
  resolved `mode`, `width`, and `height`, so a caller reads back the size the
  window manager actually gave it (which a tiling manager or a clamp may shrink).

**`set_title` updates the title bar.** Send the new `title`; the `Ok` reply
echoes the applied text. Setting a title is infallible on a real window, so on
desktop this always succeeds.

**`focus` brings the window to the front.** It takes no fields — focus is a
single imperative. The desktop chassis un-minimizes the window, shows it if
hidden, and raises and focuses it. The motivating use is `capture_frame`: a
backgrounded, minimized, or hidden window has nothing to read back, and `focus`
is the lever that foregrounds it first. (Per the platform, raising-to-front is
best-effort — the `Ok` ack means the chassis applied the calls, not that the
window manager honored every one.)

**Desktop-only, fail-fast elsewhere.** Only the desktop chassis owns a window.
The headless and hub chassis register `aether.window` too, but every handler
replies `Err` ("unsupported on this chassis — no window peripheral") immediately.
That's deliberate: a caller waiting on a window reply gets a fast, located
failure instead of a request that hangs forever on a chassis that can never
service it. The same fail-fast rule covers `capture_frame`, the other
desktop-only surface.

**The window mailbox is drained by hand, on the event-loop thread.** Window
operations have to run on the OS event-loop thread rather than a pool worker, so
the desktop chassis claims `aether.window` as an inbox the event loop drains
itself between frames. The claim hands the driver a `ClaimedInbox`, so the finish
obligation a pooled actor gets for free is carried by construction here too: each
inbound mail arrives as a guard that records `Finished` and replies along the
caller's chain when it falls out of scope, whether the op applied cleanly, the
payload failed to decode, or the kind was unrecognised. The driver applies the op
and replies; the settle rides the guard. It's the lead example of the
claimed-mailbox drain, covered in detail on
[Tracing & settlement](tracing-and-settlement.md#the-obligation-guard).

## How to use it

**From an agent over MCP.** `send_mail` rides settlement and hands back the
correlated reply, so a window change is a single call: mail `aether.window.set_title`
to `aether.window` and the applied title comes back with it. The move you'll reach
for before a `capture_frame` against a window that isn't in front is `focus`:

```text
send_mail  aether.window.focus  → aether.window   (no params)
capture_frame …
```

`describe_kinds` carries the exact param schema for each of the three kinds —
including the `WindowMode` enum's three arms — if you need to build `set_mode`
params by hand. Because these are desktop-only, run them against a desktop
substrate; on headless they reply `Err` rather than hanging.

**From a component.** `aether.window` is a chassis-owned mailbox, so a component
addresses it by name rather than through a guest-side capability facade. Send the
request kind to that mailbox and receive the reply kind like any other mail:

```rust
#[handler]
fn on_set_mode_result(&mut self, ctx: &mut FfiCtx<'_>, result: SetWindowModeResult) {
    match result {
        SetWindowModeResult::Ok { mode, width, height } => { /* the resolved state */ }
        SetWindowModeResult::Err { error } => { /* nothing changed */ }
    }
}
```

The request and reply kinds live in `aether-kinds`. Match on the reply kind to
learn the resolved state — never assume the window is exactly what you asked for.

**Boot defaults.** The initial mode and title come from `AETHER_WINDOW_MODE` and
`AETHER_WINDOW_TITLE` at boot, the same values `set_mode` / `set_title` change at
runtime. `AETHER_WINDOW_MODE` parses as `windowed` (optionally `windowed:WxH`) /
`fullscreen-borderless` / `exclusive:WxH@HZ`; an unparseable value warns and falls
back to `Windowed`. These knobs, and where they sit in the layered config, are
covered under [Configuration](configuration.md).

## How to extend or reuse it

The surface is intentionally small — three operations on one mailbox — and the
seam is the chassis. A new window operation is a new request/reply kind pair in
`aether-kinds` plus a handler on the desktop driver that applies it on the
event-loop thread and records the inbound mail `Finished`. The fail-fast
companion on the windowless chassis gains a matching `Err`-replying handler so the
op stays addressable everywhere and hangs nowhere.

The boundary not to cross is the OS toolkit itself. The window-manager specifics
churn and are platform-dependent; the mail surface — the three kinds and the
applied-value replies — is the contract a caller writes against, and it stays put
when the platform layer underneath moves.

## Where to read more

- The substrate/chassis split that makes window an opt-in desktop capability —
  [ADR-0035](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0035-substrate-chassis-split.md).
- How the claimed window mailbox settles its finish obligation by construction,
  and the guard that backs the relay seams —
  [Tracing & settlement](tracing-and-settlement.md#the-obligation-guard).
- Why a single `send_mail` returns the applied value, and the `capture_frame`
  that pairs with `focus` — [The MCP harness](../mcp-harness.md).
- Where `AETHER_WINDOW_MODE` / `AETHER_WINDOW_TITLE` sit among the config layers —
  [Configuration](configuration.md).
