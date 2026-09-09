# Changelog

Aether is pre-1.0. Alpha releases are cut from `main` and break wire formats and
APIs without a deprecation window.

## 0.4.0-alpha (unreleased)

### Highlights

- Draw a HUD without a UI framework: `aether.render.draw_shapes` evaluates rounded boxes, circles, rings and drop shadows as a signed distance field on the GPU, beside textured quads and screen-space triangles on the same overlay pass.
- Put text on screen: mail `aether.text.load_font` a TTF out of the `assets` namespace, then `aether.text.draw` a string in screen or world space — layout, glyph rasterization and atlas packing all happen inside the engine.
- Serve HTTP from an actor: `aether.http.server` registers typed routes off an `#[http::router]` impl block, upgrades to websockets, streams request and response bodies under windowed flow control, and spreads one route across replicated handler mailboxes.
- Open more than one window: `aether.window.create` gives each window its own addressable mailbox, with a real platform menu bar and cursor icons on macOS and Windows.
- Hear more than a synth: load an SFZ instrument bank or play a WAV track through `aether.fs`, place each note in the stereo image with a per-note pan, and trim one sender's sounding voices live.
- Build a screen out of widgets: `aether-kit-widget` ships buttons, tabs, dropdowns, virtual lists, sliders, text fields, dialogs, tooltips, toasts and splitters as inline child actors that composite into one ordered draw.
- Reload a component without restarting the engine: `cargo xtask dev-component` watches, cross-builds, uploads and swaps one wasm module in place, and `on_dehydrate` / `on_rehydrate` carry its state across the swap.
- The engine now hosts its own development pipeline: the bloomery chassis runs work orders as actors over the same mail, HTTP, and persistence surfaces a game would use.

### Engine

- Actor identity is a fold over lineage, not a hash of a name. `ctx.actor::<C>()` resolves a sibling at compile time and stays correct when the target is re-parented; external addresses abbreviate (`aether.window://main`).
- One wasm module can export several actors (`export!(A, B, C)`), name an entry at load time, and spawn co-located inline children that persist and reconstruct across a replace.
- Handler classes are explicit — `#[handler::single]`, `#[handler::multi]`, `#[handler::manual]` — and a handler's return type is its reply contract, recorded in a link-time manifest.
- Handler sets let one runtime block of handlers be written once and mixed into several actors.
- Actor state serializes as a declared kind (`save_state_kind`), so a hot swap carries typed state rather than opaque bytes.
- `aether-data` owns the wire format: `aether_data::wire` encodes and decodes every mail payload from the kind's `Schema`.
- `#[aether_data::kind(name = "…")]` declares a kind and its whole derive stack in one attribute.
- `aether.inventory` answers manifest and resolve queries over mail, so tagged ids render as real names.
- Chassis config is derived at compose time from the capabilities actually linked, resolved argv above env above default, and self-reported by the binary.

### Chassis and capabilities

- Every native capability is its own crate — `aether-render`, `aether-audio`, `aether-fs`, `aether-window`, `aether-text`, `aether-clipboard`, `aether-http`, `aether-tcp`, `aether-process`, `aether-component`, `aether-lifecycle`, `aether-rpc`, `aether-fleet` — and a chassis composes only the ones it needs.
- Render: 4× MSAA on the world and overlay passes, a world-space material pass, an authored render-program surface with a geometry registry, compute and indirect draws, per-pass GPU timings behind a boot knob, and device-loss recovery for offscreen targets and retained desktop windows.
- Window: per-window mailboxes under a supervising manager, plus `set_menu`, `set_cursor`, `close`, `request_redraw` and `focus`; key repeat, typed character, IME preedit, modifier, mouse-button and wheel input kinds.
- Audio: sampled SFZ banks with sustain loops, WAV track playback in its own mixer lane, timed note scheduling, per-note pan, per-sender gain, a master reverb send, and noise / pitch-sweep percussion built-ins.
- Filesystem: every request and reply addresses its file through one `addr` object, and `aether.fs.copy` takes a host path or a namespace source.
- HTTP: keep-alive, bounded concurrent connections, the peer address on each request, path templates with captures, deferred routes with a 504 obligation table, and a supervisor-plus-dispatch-shards split.
- New capabilities: `aether.process` (one-shot exec), `aether.clipboard` (real OS text clipboard on desktop), and a behavior host embedding a `wasmi` interpreter so scripts attach to any wasm actor's children. `aether.tcp` gained outbound connect and frame delivery to a bound consumer.
- Components can embed assets in wasm custom sections (`export_asset!`) and fetch them through a load-window hostcall.

### Tooling

- The MCP harness went from 11 tools to 22. New: `upload_binary`, `upload_component`, `list_binaries`, `list_components`, `pin_artifact`, `unpin_artifact`, `describe_handlers`, `describe_transforms`, `compare_component_contracts`, `actor_cost`, `collect_failure_evidence`.
- Content-addressed registries back the fleet: artifacts are stored by sha256, and `spawn_substrate` takes a selector, a boot manifest of components with their config and replica counts, and a bundle of init mail — so an engine comes up already loaded and seeded in one call.
- `capture_frame` scores checks against the raw frame, scopes them to a region, compares against a reference image, and bounds what it inlines; oversized tool responses and reply byte fields spill to a file rather than flooding the channel.
- Two in-repo test harnesses: `SubstrateHarness` (in-process, GPU, pixel readback) and `FleetHarness` (real hub, real RPC, a forked headless child).
- `cargo xtask` gained `package` (a content-addressed depot layout), `build-wasm`, `dev-component`, `affected` (reverse-dependency test selection), `symbols`, `namespaces`, `bump`, `docs check-mcp-tools` (pins the documented tool list against the registered one), and `transform verify.*` (one command per CI gate).
- The contributor and agent guide is an mdbook under `docs/guide/`, built and deployed by CI.

### Breaking changes

- **Wire format.** Mail payloads encode through `aether_data::wire` instead of postcard. A 0.3.0-alpha peer cannot decode 0.4.0-alpha bytes.
- **Mailbox ids.** An id is a fold over the actor's lineage rather than a hash of its name. `mailbox_id_from_name` is now a `clippy.toml` disallowed method.
- **`aether.fs`.** `read` / `write` / `delete` / `list` take one `addr: { namespace, path }` object instead of flat `namespace` and `path` fields, and replies echo `addr`.
- **Window control.** The `aether.control.*` kinds are gone; window mode, title and platform queries live on `aether.window`.
- **Input subscription.** There is no `aether.input` mailbox. Subscribe key / mouse / window-size on `aether.window`, tick on `aether.lifecycle`.
- **Handlers.** `#[handler]` is replaced by the explicit classes `#[handler::single]`, `#[handler::multi]`, `#[handler::manual]`.
- **Lifecycle.** `on_drop` is gone; teardown is `unwire`. The `Replaceable` opt-in is gone — `on_dehydrate` / `on_rehydrate` are defaults on every actor.
- **Crates.** `aether-capabilities` and `aether-substrate-bundle` no longer exist. Depend on the individual `aether-<cap>` and `aether-chassis-<name>` crates.
- **Binaries.** `aether-substrate`, `aether-substrate-headless` and `aether-substrate-hub` are now `aether-desktop`, `aether-headless` and `aether-hub`.
- **Spawned children.** No `AETHER_*` environment key is inherited by a forked substrate; the child's environment is built from an allowlist and its config arrives as argv.
- **Reference components.** The bundled camera and mesh components moved into `aether-kit-commons` and renamed their namespaces: `aether.camera` → `aether.kit.camera`, `aether.mesh` → `aether.kit.mesh`.
- **MCP.** `capture_frame` requires an explicit `window_id`. Mail items name their target with `address` (the old `recipient_name` still deserializes). `replace_component` takes `address` instead of `mailbox_id` and no longer accepts `drain_timeout_ms`. `describe_kinds` replaces `full` with `detail`.

### Removed

- Crates: `aether-capabilities` (dissolved into the per-capability crates), `aether-substrate-bundle` (dissolved into `aether-chassis-*`), `aether-camera` and `aether-mesh-viewer` (folded into `aether-kit-commons`), `aether-test-fixture-probe`.
- postcard as an engine dependency — the workspace owns its wire format.
- The `aether.observation.frame_stats` kind; frame verdicts are computed substrate-side by `capture_frame`.

## 0.3.0-alpha

The previous alpha. See the [`0.3.0-alpha`](https://github.com/iamacoffeepot/aether/tree/0.3.0-alpha) tag.
