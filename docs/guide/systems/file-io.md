# File I/O

> **Governing ADR:** [ADR-0041](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0041-substrate-file-io-and-namespaces.md)
> (substrate file I/O and namespaces). The contract — the four operations, the
> namespace addressing, the echo-correlated replies — is **stable**. One backend
> ships today (a local-file adapter); the trait is built to take more.

An actor never opens a file. Everything it does crosses the mail boundary, and
the filesystem is no exception: the substrate owns the disk, and an actor reaches
it by mailing a request to the `aether.fs` mailbox and handling the reply. Read,
write, delete, and list are four kinds; the bytes pass through the substrate,
never through a raw file handle the actor holds.

When you drive the engine over MCP, file I/O is how you stage what a run will
load — write a mesh DSL or a config blob into a namespace, then point a component
at the same path — and how you read back what a run produced. When you author a
capability or component, it's a request/reply exchange like any other sink: mail
`aether.fs.read`, handle the `ReadResult` that comes back.

## Why it exists

Handing a component `std::fs` would defeat the split the whole engine is built
on. The substrate owns I/O so that a loaded actor's reach is exactly the
mailboxes it can address — give it arbitrary filesystem access and that boundary
is gone. Portability pushes the same way: a wasm guest has no filesystem
primitives at all, so disk access *has* to be a request the substrate services on
the guest's behalf. Routing it through mail keeps file I/O identical in shape to
the render, audio, and other sinks — one mental model, one boundary, capabilities
isolated behind the mail wall.

Addressing by **logical namespace** rather than real path is what makes that
boundary useful instead of merely safe. An actor mails `save` / `slot1.bin`, not
`/Users/you/Library/.../slot1.bin`; the substrate resolves the namespace to a
configured root and the actor never learns where the bytes actually live. So the
same component runs unchanged whether `save` points at a home directory on a dev
box or a tmpdir in CI, and a future backend can move the bytes somewhere else
entirely without the caller noticing.

## What it does

**One mailbox, four operations.** Everything addresses the `aether.fs` mailbox.
Each request kind pairs with a reply kind that names the same operation:

| Request | Fields | Reply | `Ok` adds |
|---|---|---|---|
| `aether.fs.read` | `namespace`, `path` | `aether.fs.read_result` | `bytes` |
| `aether.fs.write` | `namespace`, `path`, `bytes` | `aether.fs.write_result` | — (ack) |
| `aether.fs.delete` | `namespace`, `path` | `aether.fs.delete_result` | — (ack) |
| `aether.fs.list` | `namespace`, `prefix` | `aether.fs.list_result` | `entries` |
| `aether.fs.copy` | `from`, `to` | `aether.fs.copy_result` | — (ack) |

Each reply is an `Ok` / `Err` enum. Every arm — including the bare `write` /
`delete` acks — echoes the request's `namespace` + `path` (or `prefix`); the
column above is what `Ok` adds on top of that echo. The `Err` arm replaces the
added data with an `FsError`.

**Three namespaces.** A request names one by its short name — `save`, not
`save://` (the double-colon form is only a convention in prose):

- **`save`** — writable, per-user persistent storage. Save games, preferences.
- **`assets`** — read-only, the content a build ships with. Textures, meshes,
  level data. A write or delete here replies `Forbidden`.
- **`config`** — writable, for component-authored configuration (keybinds and
  the like).

Each resolves to a real directory chosen at boot; that resolution, and the
`AETHER_*_DIR` knobs behind it, are covered under [Configuration](configuration.md).

**Paths are sandboxed to their namespace root.** Every path is relative to the
root the namespace resolved to. A path with a leading `/` or any `..` segment is
rejected as `Forbidden` before the backend touches disk, so `save` / `../etc/passwd`
fails closed — an actor can't escape its namespace, and never sees the real
filesystem path.

**Writes are atomic, and fill in their parents.** The local-file backend stages a
write to a sibling temporary file and renames it into place, so a crash mid-write
leaves either the old contents or the new — never a torn file. Missing parent
directories along the path are created. There's no cross-actor locking, so two
writers racing the same path resolve last-write-wins.

**`list` enumerates a directory, shallowly.** `prefix` is resolved as a directory
path under the namespace root — an empty `prefix` lists the root — and the reply's
`entries` are the bare names directly inside it, sorted, with no recursion. A
missing directory replies `NotFound`. The names come back bare, so you rebuild a
path by joining an entry back under the prefix you listed.

**Replies correlate by what they echo, not by an id.** A handler dispatches on
the reply *kind*, which on its own erases *which* request a given reply answers —
so every reply echoes the request's `namespace` and `path` (or `prefix` for
`list`) to restore that. A caller matches a reply to its request on the kind plus
those echoed fields — no correlation id, no pending-operation table, no dependence
on the order replies arrive. For `write` and `delete`, whose `Ok` arms add nothing
else, that echo *is* the result: the ack tells you a specific path landed or was
removed. The one deliberate omission is the write bytes — a `write` reply doesn't
echo them back, so persisting a megabyte still produces a small reply.

**`FsError` is one of four shapes.** `NotFound` (no such file or directory);
`Forbidden` (a read-only namespace, or a path that tried to escape its root);
`UnknownNamespace` (nothing registered under that name); or `AdapterError(String)`
(a backend failure — disk full, a rename that lost a race — with the detail
preserved as text). The first three are precise enough to branch on; the fourth
carries free-form context without locking the enum to one backend.

The cap is wired on the desktop and headless chassis. On a chassis that doesn't
run it, `aether.fs` isn't a registered mailbox, so mail to it warn-drops like any
unaddressed name.

## How to use it

**From a component.** Address the cap by type and call the operation:

```rust
ctx.actor::<FsCapability>().write("save", "slot1.bin", &bytes);
ctx.actor::<FsCapability>().read("save", "slot1.bin");
```

These are fire-and-forget; the result arrives later as its own mail, which you
receive like any other kind:

```rust
#[handler]
fn on_read_result(&mut self, ctx: &mut WasmCtx<'_>, result: ReadResult) {
    match result {
        ReadResult::Ok { path, bytes, .. } => { /* path tells you which read */ }
        ReadResult::Err { path, error, .. } => { /* branch on error */ }
    }
}
```

Because the reply echoes `namespace` + `path`, a component with several reads
outstanding tells them apart by the echoed fields — match them against whatever
state you were waiting to fill. (The request and reply kinds live in
`aether-kinds`; you can also `send` a `Read` / `Write` kind to `aether.fs`
directly instead of going through the facade.)

**From an agent over MCP.** `send_mail` rides settlement and hands back the
correlated reply, so a read is a single call: mail `aether.fs.read` to `aether.fs`
and the `ReadResult` bytes come back with it — no polling. `describe_kinds`
carries the exact param schema for each of the four kinds if you need it.

The move you'll reach for most is `write`: stage a file the engine will then
load. Writing a mesh DSL to a namespace and pointing the mesh viewer at the same
path is the canonical loop — author the bytes, then send the load — and the same
shape covers dropping a config blob a component reads at startup. The namespaces
resolve to real directories you set per-spawn through `AETHER_SAVE_DIR` /
`AETHER_ASSETS_DIR` / `AETHER_CONFIG_DIR` (or the matching `--save-dir` flags), so
you also control where those bytes land on disk.

## How to extend or reuse it

The seams are the namespace table and the backend trait.

- **A new namespace** is registered at boot, chassis-side: build an adapter,
  register it under a short name, and it's addressable as just another
  `namespace`. The three above are the substrate's defaults; chassis code can add
  more.
- **A new backend** is an implementation of `FileAdapter` — four methods
  (`read` / `write` / `delete` / `list`), each returning `FsResult`. The trait is
  deliberately small so a bundled-asset or cloud backend is "implement four
  methods," not a rewrite of the sink. The local-file adapter is the one that
  ships; it's also the reference for what path-safety and atomicity an adapter is
  expected to enforce.

The mail surface doesn't change when a backend does. A component addressing
`save` / `slot1.bin` is untouched whether those bytes live on local disk or
somewhere a future adapter puts them — which is the entire point of resolving by
namespace rather than path.

## Where to read more

- The transport, the namespace-to-adapter design, and the precedence of config
  layers —
  [ADR-0041](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0041-substrate-file-io-and-namespaces.md).
- Where the namespace roots come from and how to set them —
  [Configuration](configuration.md).
- Why a single `send_mail` returns the read's bytes — the settlement contract on
  [Tracing & settlement](tracing-and-settlement.md), and the tool surface on
  [The MCP harness](../mcp-harness.md).
- How a component receives a reply kind in a `#[handler]` —
  [Components & lifecycle](components.md).
