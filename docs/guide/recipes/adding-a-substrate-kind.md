# Adding a mail kind

**Class:** recompile. A kind is a public wire contract, so choose its owner and
compatibility posture before writing the struct.

The current default is: **the actor/capability that exchanges the kind owns
it** (ADR-0121). `aether-kinds` is for genuinely substrate-wide vocabulary or a
documented upstream consumer that cannot depend on the owner.

## 1. Choose the owner

| Contract | Location |
|---|---|
| Request/reply for one native capability | `aether-<cap>/src/kinds.rs` |
| Component/guest public API | that component's public rlib or sibling contract crate |
| Shared lifecycle/control/diagnostic vocabulary | `aether-kinds`, with an explicit reason |
| Internal worker/task wake kind | private next to the runtime state |

Do not promote a kind merely so MCP can encode it. Live `ListKinds` reads the
engine registry, including dynamically loaded component kinds.

The clipboard contract is a compact current exemplar:

- kinds: `aether-clipboard/src/kinds.rs`;
- identity/helpers: `clipboard/mod.rs`;
- real/headless handlers: `clipboard/{runtime,headless_runtime}.rs`;
- live discovery: inventory registry → `describe_kinds`/`describe_handlers`.

## 2. Declare request and reply

```rust
use serde::{Deserialize, Serialize};

/// Request the current UTF-8 clipboard text.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.clipboard.get_text")]
pub struct GetClipboardText;

/// Reply to `GetClipboardText`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.clipboard.get_text_result")]
pub enum GetClipboardTextResult {
    Ok { text: String },
    Err { error: String },
}
```

The canonical name is the operator-facing `kind_name`. Follow the owner's
family. Derive `Kind` and `Schema`; serde derives provide the structured wire
shape. Write docs for a caller, including reply, units, limits, and error
meaning.

Reply is a handler contract, not a `Kind` associated type. Name/result shapes
should make the pairing clear, but live handler inventory is what declares the
actual reply.

## 3. Re-export the marker surface

The owner module normally exposes kinds from its always-on/marker face:

```rust
pub mod kinds;
pub use kinds::*;
```

Keep the kind's dependencies wasm-safe when guest code must address it. Native
adapter/runtime dependencies belong behind the runtime feature, not in
`kinds.rs`.

`#[derive(Kind)]` submits a native descriptor automatically. There is no manual
master vector to edit. The live engine's registry and inventory expose it.

## 4. Add a handler class deliberately

```rust
#[handler::single]
fn on_get_text(
    state: &mut Self::State,
    _ctx: &mut NativeCtx<'_>,
    _request: GetClipboardText,
) -> GetClipboardTextResult {
    match state.backend.get_text() {
        Ok(text) => GetClipboardTextResult::Ok { text },
        Err(error) => GetClipboardTextResult::Err { error },
    }
}
```

Use:

- `single` for zero-or-one typed return;
- `multi` for a declared repeated reply element;
- `manual` only when reply timing/type cannot be expressed as a return;
- `task` completion for sanctioned off-thread work.

Do not copy an old low-level `send_reply` signature. Returning the reply from a
single handler preserves correlation and keeps handler inventory accurate.

If several chassis claim the same namespace, add the contract everywhere. An
unsupported backend should return the ordinary error reply rather than accept
mail and never settle.

## 5. Add sender ergonomics when repetition warrants it

A mailbox extension trait can lift repeated construction:

```rust
pub trait ClipboardMailboxExt {
    fn get_text(&self);
}
```

Implement it for wasm and native mailbox types as appropriate. Keep the raw
kind public; the helper is ergonomics, not a second protocol.

## 6. Verify the boundary

1. **Shape tests:** encode/decode structured variants and reject malformed
   input at the owning data layer.
2. **Descriptor uniqueness:** ensure no canonical-name collision.
3. **Handler test:** exercise success and every meaningful error backend.
4. **Unsupported chassis:** prove a bounded error when the public mailbox is
   installed without the resource.
5. **Live discovery:** narrow `describe_kinds` to the exact name and
   `describe_handlers` to the owner.
6. **End-to-end mail:** SubstrateBench `send_and_await` and decode the typed result.

Use FleetBench only if the contract itself crosses RPC/process boundaries.

## 7. Treat schema edits as compatibility changes

The canonical kind id includes the canonical name/schema. A shape change can
mint a different id. Rebuild every producer and consumer, including prebuilt
wasm. Decide whether old and new forms must coexist for a migration window.

Check enum variants, optional fields, units, byte limits, and feature tiers—not
only whether Rust callers compile together in one workspace.

## When `aether-kinds` is correct

Some inventory, engine/component control, lifecycle, trace/log/cost, window
driver, and utility contracts remain upstream because substrate/MCP/bundle
consumers need them without depending on capability implementation. Document
that dependency reason beside the family. Central placement is an exception
with an owner, not the default for “native.”

Continue with [Capability anatomy](../capability-anatomy.md),
[The type system](../foundations/type-system.md), and
[Inventory and transforms](../systems/inventory-and-transforms.md).
