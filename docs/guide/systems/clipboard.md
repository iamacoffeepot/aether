# Clipboard

`aether.clipboard` is a small request/reply capability for UTF-8 text. It is
separate from input streams: input publishes user events, while clipboard
operations explicitly request or replace shared peripheral state.

## Public contract

| Request | Reply | Meaning |
|---|---|---|
| `aether.clipboard.get_text` | `aether.clipboard.get_text_result` | read current text |
| `aether.clipboard.set_text` | `aether.clipboard.set_text_result` | replace current text |

Both reply enums have explicit `Ok`/`Err` arms. Once a backend has initialized,
a failed get/set is therefore an ordinary capability result, not a
missing-response convention. Initialization is a separate boundary: creating
the `System` backend happens during actor boot, and failure there returns
`BootError` before any clipboard request can be handled. `ClipboardMailboxExt`
provides typed helpers for wasm and native callers.

## Chassis behavior

The capability has two backend modes:

- `System` uses the operating-system clipboard;
- `InMemory` stores deterministic process-local text for tests.

Desktop composes the system backend. SubstrateHarness uses the in-memory backend by
default. If the desktop cannot create the OS clipboard during capability init,
the chassis build fails; that case does not become a `get_text_result::Err`.
A chassis without clipboard support can instead install
`HeadlessClipboardCapability`, which owns the same namespace and replies with
errors immediately. This is preferable to silently dropping requests or making
callers special-case mailbox absence.

Verify actual installation in the chassis builder or with
`describe_handlers`. The existence of marker types under a feature means code
can address the capability, not that every process has a working OS backend.

## Security and correctness boundary

Clipboard text is ambient user data:

- do not log contents by default;
- do not poll continuously as an input mechanism;
- propagate an error result rather than treating it as empty text;
- make writes an intentional user-facing action;
- keep arbitrary binary payloads out of this text-only contract.

OS clipboard APIs may fail because of platform integration, display-session
availability, contention, or unsupported formats. These are adapter failures,
not actor routing failures. Failures after successful initialization become the
typed `Err` replies above; failure to construct the system adapter is a chassis
boot error instead.

## Extending the capability

Do not overload the text kinds with images or platform-specific flavor ids.
Adding a new data class needs its own schema and explicit cross-platform
fallback contract. Preserve the fast-fail headless behavior so settlement never
waits on a request that cannot complete.

## Change route

- Marker and typed helpers: `crates/aether-clipboard/src/mod.rs`
- Kinds: `crates/aether-clipboard/src/kinds.rs`
- System/in-memory runtime: `crates/aether-clipboard/src/runtime.rs`
- Unsupported runtime: `crates/aether-clipboard/src/headless_runtime.rs`
- Backend selection: `crates/aether-clipboard/src/config.rs`
- Chassis installation: `crates/aether-chassis-{desktop,headless,harness}/`
