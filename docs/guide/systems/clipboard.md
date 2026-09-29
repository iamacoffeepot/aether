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
`BootError` before any clipboard request can be handled.

A caller that declares `depends(ClipboardCapability)` sends the request kind
directly; the reply arrives at its handler for the matching result kind:

```rust
ctx.send::<ClipboardCapability>(&GetClipboardText);
```

## Chassis behavior

The capability has two backend modes:

- `System` uses the operating-system clipboard;
- `InMemory` stores deterministic process-local text for tests.

Desktop composes the system backend. SubstrateHarness uses the in-memory backend by
default. If the desktop cannot create the OS clipboard during capability init,
the chassis build fails; that case does not become a `get_text_result::Err`.
A chassis without clipboard support composes no clipboard actor, so a
component that declares `depends(ClipboardCapability)` is refused at load there,
naming `aether.clipboard` as the dependency that is not live; callers never
special-case mailbox absence.

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
fallback contract. A request the backend cannot complete answers its `Err` arm
so settlement never waits on it.

## Change route

- Marker and typed helpers: `crates/aether-clipboard/src/lib.rs`
- Kinds: `crates/aether-clipboard/src/kinds.rs`
- System/in-memory runtime: `crates/aether-clipboard/src/runtime/mod.rs`
- Backend selection: `crates/aether-clipboard/src/config.rs`
- Chassis installation: `crates/aether-chassis-{desktop,harness}/`
