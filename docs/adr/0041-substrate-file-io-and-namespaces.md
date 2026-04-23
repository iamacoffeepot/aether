# ADR-0041: Substrate file I/O with namespaced adapters

- **Status:** Proposed
- **Date:** 2026-04-23

## Context

Components today cannot touch the filesystem. Everything that crosses the wasm boundary goes through the mail system; I/O has no sink. That's fine for the pure-compute pipeline the substrate has been to date, but it forecloses three real forcing functions:

- **Save games.** ADR-0016 persists component state across `replace_component`, and ADR-0040 layered kind-typed framing on top, but neither touches disk — a crash or shutdown loses everything. "Write the save bundle to a file" has no path today.
- **Asset loading.** Textures, audio, level data. The desktop chassis already synthesises audio (ADR-0039) and draws triangles; the next logical step is loading content authored out-of-process, which means reading bytes from disk.
- **Configuration.** The substrate itself already consumes env vars (`AETHER_WINDOW_MODE`, `AETHER_AUDIO_*`, `AETHER_TICK_HZ`, `AETHER_LOG_FILTER`). A growing component surface wants the same knob, and env-var-only breaks down once a component wants to ship a structured config.

Giving components `std::fs` access is a non-starter — it hands them arbitrary filesystem reach, defeats the "substrate owns I/O" invariant that motivated the wasm-on-substrate split (ADR-0002), and tangles portability (wasm32 has no filesystem primitives at all). The path forward is a substrate-mediated I/O sink, shaped consistently with the existing sink pattern (ADR-0039 audio, ADR-0008 render) so components gain no new capabilities — the substrate just learns to resolve one more recipient.

This ADR decides the shape: transport (mail-based), addressing (logical namespaces → adapter mappings), operations (read/write/delete/list), and the configuration layering that resolves what `save://` actually means on a given host.

## Decision

### 1. Transport: mail on an `"io"` sink

All filesystem access goes through four mail kinds on the substrate-owned `"io"` sink, reply-required in the ADR-0013 sense (components mail a request, the sink replies to the originating sender with a result). No new host-fn surface; the FFI is exactly what ships today.

```rust
// Inbound requests (cast or postcard per ADR-0019; all carry a
// namespace + path pair)
aether.io.read   { namespace: String, path: String }
aether.io.write  { namespace: String, path: String, bytes: Vec<u8> }
aether.io.delete { namespace: String, path: String }
aether.io.list   { namespace: String, prefix: String }

// Reply kinds routed back via reply_mail
aether.io.read_result   : Ok { bytes: Vec<u8> } | Err { error: IoError }
aether.io.write_result  : Ok | Err { error: IoError }
aether.io.delete_result : Ok | Err { error: IoError }
aether.io.list_result   : Ok { entries: Vec<String> } | Err { error: IoError }

enum IoError {
    NotFound,
    Forbidden,            // path escaped the namespace root
    UnknownNamespace,
    AdapterError(String), // backend-specific detail preserved as text
}
```

`namespace` is the logical prefix without the `://`: mail carries `"save"`, not `"save://"`. The double-colon form is only a UX convention in docs and log lines. Reply kinds pair 1:1 with requests so callers can match on the specific result type.

Mail-based transport means reads allocate (adapter→`Vec<u8>`→postcard→delivery→decoded component-side `Vec<u8>`). Fine for save files and config; wasteful for large assets. Deferred: a host-fn fast path for zero-copy reads into component memory. Not blocking for v1.

### 2. Addressing: logical namespaces → adapter backends

The substrate holds a boot-time `HashMap<String, Arc<dyn FileAdapter>>`. Well-known namespaces:

- `save://` — writable, per-user-per-engine persistent storage (save games, preferences).
- `assets://` — read-only, ships with the substrate binary (textures, audio, level data).
- `config://` — readable and writable by the substrate only during boot; writable by components at runtime for component-authored config (user keybinds, etc).

Additional namespaces can be registered at boot by chassis code. The three above are the substrate-owned defaults; a future cloud-save adapter would register itself as a fourth (e.g. `cloud-save://`) rather than hijacking `save://`.

Path semantics: every path is relative to the namespace root; `..` and absolute prefixes are rejected as `Forbidden`. Components never see the real filesystem path — they mail `save://slot1.bin` and the substrate resolves it.

### 3. Adapter trait

```rust
trait FileAdapter: Send + Sync {
    fn read(&self, path: &str) -> Result<Vec<u8>, IoError>;
    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), IoError>;
    fn delete(&self, path: &str) -> Result<(), IoError>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, IoError>;
}
```

v1 ships one impl: `LocalFileAdapter { root: PathBuf, writable: bool }`. Writes are atomic — stage to `{path}.tmp-{pid}-{nonce}`, `fsync`, `rename`. `list` is shallow and prefix-filtered (no recursion — callers that want a tree walk paginate themselves).

Future adapters (not in v1): `BundledAdapter` (reads from a zip or `include_bytes!` blob), `S3Adapter` (cloud), `MemoryAdapter` (test harness, in-memory). The trait is deliberately small so adding another backend is "impl four methods" rather than "refactor the sink."

### 4. Configuration and precedence

Substrate configuration resolves through four layers, **highest precedence first**:

1. **CLI arguments** (e.g. `--save-dir=/tmp/saves`) — not wired today; deferred until a dev-ergonomics case actually asks for it. The wire is ready (`spawn_substrate` already accepts `args: []`).
2. **Environment variables** (`AETHER_SAVE_DIR`, `AETHER_ASSETS_DIR`, `AETHER_CONFIG_DIR`) — v1 ships here. Matches existing substrate convention (`AETHER_WINDOW_MODE`, `AETHER_AUDIO_*`, `AETHER_TICK_HZ`, `AETHER_LOG_FILTER`).
3. **Config file** (TOML at `$AETHER_CONFIG` or default `~/.config/aether/aether.toml`) — not wired today; deferred until a second adapter type exists, at which point env-var cardinality explodes and TOML's structural shape wins. Expected TOML form:
   ```toml
   [namespaces.save]
   adapter = "file"
   path = "$XDG_DATA_HOME/aether/save"

   [namespaces.save-cloud]
   adapter = "s3"
   bucket = "my-game-saves"
   region = "us-west-2"
   ```
4. **Defaults** — via the `dirs` crate, platform-correct: `save://` → `data_dir()/aether/save`, `assets://` → `exe_dir()/assets`, `config://` → `config_dir()/aether`.

The precedence order is the commitment; which layers exist on day one is a separate shipping question. Adding TOML or CLI later is additive — the substrate reads in priority order, missing layers collapse to the next one down, and components never see the source.

### 5. Self-reference: the config bootstrap

`config://aether.toml` (once TOML ships) is self-referential — the substrate needs to resolve `config://` to read the file that defines `config://`. Handled by pinning the TOML's location *outside* the namespace machinery: `$AETHER_CONFIG` env var or a hardcoded default path resolves the bootstrap TOML directly from the filesystem, and everything downstream goes through the namespace resolver. Same trick `/etc/fstab` uses — the fstab file itself lives at a pinned path that doesn't depend on the mounts it defines.

### 6. Chassis coverage

- **Desktop chassis**: full I/O sink, all four namespaces, local-file adapter.
- **Headless chassis**: full I/O sink, same as desktop (headless builds still run integration tests that touch disk). Defaults may differ (`assets://` often points at a test-fixture dir, not the binary-adjacent one).
- **Hub chassis**: no I/O sink — the hub is a coordination plane, not a content host. Mail addressed to `"io"` on a hub-chassis substrate gets an `UnknownNamespace` reply so callers fail loud rather than hanging.

## Consequences

### Positive

- **Sandbox by construction.** Components can never reach outside their declared namespace. Path normalization + `Forbidden` rejection at the adapter layer; no component sees a real filesystem path.
- **Adapters compose with deployment.** Swapping local-file for cloud or bundled is substrate-side config. Components addressing `save://slot1.bin` don't change; the bytes just live somewhere else.
- **Save/load falls out of ADR-0040.** A component that calls `save_state_kind::<MyState>(&self.state)` today and wants to persist across restart mails `write { namespace: "save", path: "mystate.kind", bytes }` — same `[K::ID | payload]` framing. Load is the inverse; `as_kind::<MyState>()` on the loaded bytes works identically to the replace-component path.
- **Consistent with existing sinks.** ADR-0039's audio sink, ADR-0008's render sink, and this I/O sink share one mental model: substrate-owned recipient, mail in, reply out, capabilities isolated behind the mail wall.
- **Testable.** `MemoryAdapter` (future) gives integration tests a drop-in backend with no disk touched; `LocalFileAdapter` rooted at a `tempdir()` works today without extra scaffolding.

### Negative

- **Mail-path allocation.** Reads copy bytes multiple times — adapter output → postcard-encoded mail payload → decoded `Vec<u8>` on the component side. Fine at save-file size (KB-MB); wasteful at asset size (tens of MB). Flagged as a follow-up host-fn fast path; not v1.
- **Namespace mapping is new substrate state.** Today the substrate holds mailbox registry + kind registry; this adds a third. Small but worth noting — tests that spin up a substrate now need to seed the adapter map.
- **Config surface grows.** Three env vars on day one (`AETHER_SAVE_DIR` / `AETHER_ASSETS_DIR` / `AETHER_CONFIG_DIR`) and an eventual TOML file. Each layer is well-bounded, but the total complexity of "where does `save://` live" climbs.
- **Write semantics are last-write-wins.** Two components writing the same path race through the tmp-rename to atomicity on the final file, but with no locking — whoever renames second clobbers the first. Acceptable today (mail queue is serialized per engine; true contention needs two engines on the same namespace, which isn't a v1 shape), noted for future multi-engine work.

### Neutral

- **Host fn surface unchanged.** Everything rides the existing mail FFI. No new `_p32` import, no wasm custom section, no schema wire change.
- **Component SDK unchanged.** Components `ctx.send(&io_sink, &Read { ... })` and handle `ReadResult` through a `#[handler]` — the same shape they already use for any other sink reply. An SDK helper (`ctx.read("save://slot1.bin") -> async Result<Vec<u8>>`) is worth adding once the primitive lands but isn't load-bearing for v1.
- **Hub chassis stays pure.** I/O is a desktop-and-headless feature; the hub remains a coordination plane. Same pattern as ADR-0039 audio (desktop-only) and the render/camera sinks (desktop-only).

## Alternatives considered

- **Host-fn-first (WASI-style).** `aether::read_file_p32(ns_ptr, ns_len, path_ptr, path_len, out_ptr, out_cap) -> u32`. Synchronous, zero Vec allocation, matches what game devs expect from the C/Rust ecosystem. Rejected for v1: breaks the "everything is mail" consistency the substrate has committed to, and the allocation cost only matters at asset size (not save/config). Reopen as a targeted fast path when asset streaming forces it.
- **Absolute paths.** Components pass `/Users/.../save/slot1.bin` directly. Rejected: no sandbox, no deployment portability (paths break across machines), no way to swap backends.
- **Mail-driven namespace registration by a config component.** Ship the substrate with zero namespaces; a dedicated "config" wasm component registers them at boot. Rejected on bootstrap paradox — the config component needs `assets://` to load, and `assets://` only exists after the component runs. Chassis-side registration in native code sidesteps the cycle.
- **Ship cloud / bundled adapters on day one.** Rejected as YAGNI. One adapter (local file) covers the v1 forcing functions (save games, config, asset loading from disk). The trait makes adding a second adapter a bounded PR, not a rewrite.
- **Single `aether.io.op` kind with a `kind: Op` enum.** Multiplexing read/write/delete/list through one kind. Rejected — four kinds are cheap, reply-kind matching is cleaner, and the hub's `describe_kinds` surface is more useful with distinct kinds than a single polymorphic one.
- **Filesystem-per-namespace via chroot/unshare.** Kernel-level sandbox instead of path-normalization. Rejected — platform-specific (Linux-only without macOS work), overkill for the threat model (components are signed code the engine owns, not adversarial), and kills testability.

## Follow-up work

- **PR**: substrate-side — define `FileAdapter` trait, ship `LocalFileAdapter`, wire the `"io"` sink to dispatch the four kinds, add env-var config resolution and `dirs`-crate defaults.
- **PR**: kinds — add the eight kinds (four requests + four replies) to `aether-kinds`, plus the `IoError` shape.
- **PR**: component SDK ergonomics — `ctx.read(namespace, path)` / `ctx.write(...)` helpers that wrap the mail send + reply handler so components don't hand-roll the envelope.
- **Parked, not committed**: host-fn fast path for large reads (`read_file_p32` into a pre-allocated component buffer), once asset streaming forces it.
- **Parked, not committed**: TOML config file + bootstrap resolver, once a second adapter type ships.
- **Parked, not committed**: CLI argument parser, once dev-ergonomics (running a substrate by hand) actually asks for it.
- **Parked, not committed**: `BundledAdapter` reading from an `include_bytes!` archive so shipped builds bundle `assets://` into the binary.
- **Parked, not committed**: SDK sugar on ADR-0040 — `ctx.save_to_disk::<K>(namespace, path, &state)` that combines kind-typed framing with the I/O sink, closing the loop the "file serialization layer" discussion pointed at.
