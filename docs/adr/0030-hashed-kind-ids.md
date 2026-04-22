# ADR-0030: Schema-hashed kind ids

- **Status:** Accepted
- **Date:** 2026-04-20

## Context

ADR-0005 shipped registry-at-init kind ids: the substrate assigns a sequential `u32` at boot (plus any runtime additions from ADR-0010's load path), and guests resolve names to ids via the `resolve_kind_p32` host fn during `init`. Two follow-on ADRs locked the vocabulary in place — ADR-0027 (`type Kinds = (...)` typelist) and ADR-0028 (per-component manifest in a wasm custom section). Kind ids themselves stayed session-local integers the substrate chose.

ADR-0029 just moved mailbox ids to a deterministic hash of the mailbox name. Same pressure points — cross-process comparability, eliminating a host-fn round trip, shrinking registry bookkeeping — apply equally to kinds. The hash machinery is already in-tree: `aether_mail::mailbox_id_from_name` is a `const fn` FNV-1a 64. Applying the same pattern to kinds finishes the story and retires the second of the two "resolve foo by name" host fns.

**But kinds aren't just routing tokens.** A mailbox is a pure address — the `kind` field on the mail envelope carries the wire-shape discriminator, so two mailbox ids can match across versions without risk. A kind id, by contrast, *is* the wire-shape discriminator. If two builds agree that `id = hash("foo.thing")` without caring about the shape of `thing`, a producer that emits `Thing v1 { a: u8 }` and a consumer expecting `Thing v2 { a: u8, b: u8 }` route mail under the same id and silently garbage-decode. Name alone is the wrong input.

Schema-including hashing surfaces drift as a loud miss instead. If the input is `name + postcard(schema)`, a schema change produces a different id. The stale side's `resolve_kind` (or `K::ID` compile-time constant) doesn't match what the fresh side registered; the stale sender's mail lands on "kind not found"; the stale receiver's `mail.is::<K>()` returns false. Each of those is an obvious failure mode with a clear fix (recompile), not a silent data-corruption hazard.

This ADR commits to (a) hashing `name + postcard(schema)` rather than name alone, (b) widening the kind id to 64 bits, (c) emitting the id as a `const ID: u64` on the `Kind` trait at derive time (so guests never call a host fn to resolve it), and (d) retiring `resolve_kind_p32`. The machinery slots into the shape ADR-0028 already established — the derive already has the schema at expansion time, already has access to `postcard::to_allocvec`, and already emits a `[u8; N]` static. Adding a `const ID` is the same work at the same expansion site.

### Input stream auto-subscribe

One side-effect currently rides on `resolve_kind_p32`: if the resolved kind is one of the substrate's input streams (`aether.tick` / `aether.key` / …), the host fn adds the caller's mailbox to that stream's subscriber set (ADR-0021). Retiring the host fn means that path has to move.

The natural home is the guest's SDK: `KindList::resolve_all` walks the typelist at init anyway, and `K::IS_INPUT` is already a compile-time const on the trait (ADR-0021's derive flag). The SDK sends an `aether.control.subscribe_input` mail for each input kind in the typelist, using the already-public subscribe path. No new host fn; no lost behavior.

## Decision

**Kind ids become 64-bit and derived from `name + postcard(schema)` at compile time. `resolve_kind_p32` is retired. Auto-subscribe moves from the host fn to the guest SDK's init walker, which emits `subscribe_input` for every `K::IS_INPUT` kind in `Component::Kinds`.**

### Hash input

The hash input is `KIND_DOMAIN ++ name_bytes ++ postcard(schema)` (domain prefix added as a follow-up, issue #186):

- **Domain tag**: the literal ASCII bytes `b"kind:"`, stored as `aether_mail::KIND_DOMAIN`. Disjoins the `Kind::ID` space from `MailboxId` (which prefixes `b"mailbox:"`) so a future code path that confuses the two ids can't misattribute by construction.
- **Name bytes**: the UTF-8 bytes of `K::NAME` (unchanged from ADR-0029's mailbox shape).
- **Schema bytes**: `postcard::to_allocvec(&K::schema())`, the same encoding ADR-0028 records already carry. `SchemaType` is the canonical shape vocabulary — changing a field name, type, or layout changes the postcard bytes, therefore changes the id.

`postcard` is chosen over a handcrafted schema-serialization because (a) it already ships in-tree, (b) ADR-0028 already commits to it as the wire shape for kind descriptors, and (c) its output is deterministic given a stable struct definition (small-varint, no ordering nondeterminism). The only way two kinds collide on the id is if they have identical names *and* identical schemas — at which point they're the same kind, which is the behavior we want.

### Hash function

FNV-1a 64, same algorithm as `aether_mail::mailbox_id_from_name`. Same justification (no-dep, deterministic, fast, distribution is fine at kind cardinality). Emitting a second algorithm would be churn without benefit; the two id spaces are disjoint by construction (domain prefixes make the inputs disjoint) so there's no risk of cross-talk.

`MailboxId::from_name(name)` is `fnv1a(MAILBOX_DOMAIN ++ name)`. `kind_id_from_parts(name, schema_bytes)` is `fnv1a(KIND_DOMAIN ++ name ++ schema_bytes)` — same algorithm, different domain-prefixed inputs. Both live in `aether-mail` and are `const fn` so derive-time use is cheap.

### Derive output

`#[derive(Kind)]` gains a `const ID: u64` on the `Kind` trait:

```rust
pub trait Kind {
    const NAME: &'static str;
    const ID: u64;
    const IS_INPUT: bool = false;
}
```

The derive, which already computes the `KindDescriptor` at expansion time (ADR-0028), additionally computes `fnv1a(name ++ postcard(descriptor.schema))` and emits it as a literal `const ID: u64 = 0x...` on the generated `impl`. No `const fn postcard` required — the bytes exist as compile-time `Vec<u8>` values during expansion, and the hash is folded to a `u64` literal.

`aether-kinds` reuses this — every kind in the control-plane vocabulary picks up the const automatically via its existing `#[derive(Kind, Schema)]`.

### Substrate registry

`Registry` widens `kind_by_name: HashMap<String, u32>` + parallel `kind_names: Vec<String>` + `kind_descriptors: Vec<KindDescriptor>` to a single `HashMap<u64, KindSlot { name, descriptor }>` keyed on the derived id. Registration:

1. Compute `id = fnv1a(name ++ postcard(descriptor.schema))`.
2. `match mailboxes.get(&id)`:
   - Occupied with matching descriptor → idempotent, return the id.
   - Occupied with mismatching descriptor → `KindConflict`. Given the hash input includes the full schema, this is a collision, not a schema drift — vanishingly rare, hard error.
   - Empty → insert.

`kind_id(name)` and `kind_name(id)` stay on the public surface (observability, logging, sink handler hand-off) but now cost a HashMap lookup rather than a vec index. The `by_name` map is rebuilt as part of the struct; there's no second map because names aren't the routing key any more.

### Wire changes

- `aether-substrate::mail::MailKind`: `u32` → `u64`. `Mail.kind` field widens. FFI fields that carry kind ids (`send_mail_p32`'s `kind`, `reply_mail_p32`'s `kind`, `receive_p32`'s `kind`) widen to `i64` at the wasm ABI. Scalar, not pointer — the `_p32` suffix still applies for the same reason it did for mailbox ids.
- `aether-kinds::descriptors::all()` unaffected — the return type is `KindDescriptor`, which is name+schema. The substrate computes ids from those.
- Host fns: `resolve_kind_p32` is removed. `KIND_NOT_FOUND` constant removed.
- Guest SDK: `resolve::<K>()` becomes `KindId { raw: K::ID, _k: PhantomData }` — pure const-construction, no FFI. `KindTable` populated from `K::ID` at the typelist walker.

### Auto-subscribe

The SDK's `KindList::resolve_all` (ADR-0027) already walks every `K` in `Component::Kinds`. Today it calls `resolve::<K>()`, which hits the host fn. Under this ADR the walker:

1. Installs `(TypeId::of::<K>(), K::ID)` into the per-component `KindTable`. No host fn.
2. If `K::IS_INPUT`, sends an `aether.control.subscribe_input { stream: <lookup>, mailbox: <self> }` mail via `send_mail`.

Step 2 replaces the substrate-side side-effect that today fires inside `resolve_kind_p32`. Slightly more guest code per input kind (three to four wasm instructions building and sending the subscribe mail), but the code path is already warm — `subscribe_input` is the same mail `ControlPlane` handles today. The `<lookup>` is a compile-time `match` on `K::NAME` against the four input-kind names — the SDK already has `input_stream_for_name` on the substrate side; we port the same match to the SDK side.

An advantage falls out: a guest that declares `type Kinds = (Tick, ...)` no longer relies on the substrate being around at init time to subscribe — the subscription is just mail that gets queued and handled whenever the control plane picks it up. This composes more cleanly with ADR-0022's drain-on-swap (a replaced component's new instance's subscriptions land in the same pending-mail order as any other boot activity).

### Substrate boot + ADR-0010 load flow

Boot: `main.rs` already iterates `aether_kinds::descriptors::all()` and calls `register_kind_with_descriptor`. Each call now derives the id from the descriptor instead of allocating sequentially; the registry stores under the derived id. The descriptor list order no longer determines ids — builds of the substrate produce the same id for the same kind.

`load_component` (ADR-0028): the substrate reads the embedded manifest (list of `KindDescriptor`), registers each via the same `register_kind_with_descriptor` path. Conflict detection stays exactly as today's — a kind already registered under a different schema surfaces a `KindConflict`; one under the identical schema is idempotent. What changes: the conflict is now possible only on a genuine hash collision (two different schemas producing the same id), which at 64 bits is not a concern we design for.

### Out of scope

- **Renaming `MailKind`.** The type alias becomes `pub type MailKind = u64` — no rename, no reshape. Every kind-id touchpoint on the wire just widens its integer.
- **Cross-substrate routing.** Like ADR-0029, this makes the id space portable without prescribing a routing protocol.
- **Removing `KindDescriptor` from the runtime surface.** The descriptor is still the hub's way to encode agent params, still the manifest payload (ADR-0028). Only the id derivation changes.

## Consequences

- **Schema drift is loud.** A producer and consumer compiled against different versions of the same kind disagree on `K::ID`. The stale one's mail lands on "kind not found" (or its `decode_typed` returns `None`) — visible, fixable, not a silent miscompile-with-network-steps.
- **Collision horizon in persistent logs.** The relevant *n* is cumulative distinct `(name, schema)` pairs ever recorded, not live kinds — every schema revision spawns a new id. Safe zone is ~190k cumulative revisions at 1-in-a-billion, ~6M at 1-in-a-million; a single project would have to churn 100k new kind revisions per day for a century to hit 50%. And the log collision story has a backstop: a collision means "two log entries share an id but are different kinds," and the hub/substrate already stamp `kind_name` alongside ids in observation and sink-handler paths, so `(id, name)` pairs remain unambiguous even if bare ids ever clash. The 64-bit width is sized for "bare id uniqueness in realistic lifetimes" with "name disambiguates everywhere it matters" as the defense in depth.
- **One fewer host fn.** `resolve_kind_p32` joins `resolve_mailbox_p32` in the retired pile. Both lookup-by-name host fns are gone; ids are a function of compile-time inputs on both sides of the FFI.
- **Ids are meaningful across processes and sessions.** Kind ids logged in one substrate run identify the same logical kind in any other run that has the same kind defined. Observability dashboards and log comparisons become stable without a kind-name join.
- **Breaking wire change.** `MailKind` widens `u32` → `u64` across every envelope and FFI. Single coordinated PR pair, pre-1.0.
- **Registry shrinks.** Parallel `kind_names` + `kind_descriptors` vecs + `kind_by_name` map collapse into one `HashMap<u64, KindSlot>`.
- **Guest SDK gets three lines of subscribe plumbing per input kind.** Trivial cost; the code path is already in use.
- **`const ID` joins `const NAME` as a required `Kind` trait member.** The `derive(Kind)` macro populates it; hand-rolled `impl Kind` has to compute the same hash or mismatch the substrate. Guest-side `aether-mail` exposes `fnv1a_kind_id(name, schema_bytes)` for the hand-rolled case.
- **ADR-0005's "kind names are the stable contract, ids are opaque" guidance no longer needs the "resolve at init" framing.** Names stay stable; ids are now a derivable function of names + schemas. `K::NAME` stays public; `K::ID` joins it as equally stable (for a fixed schema).

## Alternatives considered

- **Hash name alone, like mailbox ids.** Rejected: loses the drift-surfacing property. A schema change without a name change silently reuses the id; mail routes fine but decodes to garbage. The whole point of moving to hashed kind ids is that kinds carry wire shape in a way mailboxes don't.
- **Leave kind ids sequential, keep `resolve_kind_p32`.** Rejected: the host fn round-trip is the one we most want to retire (per-kind, per-boot, for every component). The "what if we want to remove one but not the other" question is settled by ADR-0029 setting the precedent and this ADR completing it.
- **Use a cryptographic hash (Blake3, SHA-256 truncated).** Rejected, same reason as ADR-0029: kinds aren't adversarial input, and non-crypto hashes compile to fewer bytes in wasm.
- **Hash width 128 bits.** Rejected: the birthday-bound argument from ADR-0029 applies; 64 bits is well past the "never happens in this project's lifetime" regime for kinds. Wider wire for no benefit.
- **Include a version byte in the id (not the schema).** Rejected: schema inclusion already achieves what version bytes approximate, and it's automatic — no manual "bump the version" discipline. A version byte would be error-prone (developers forgetting to bump).
- **Keep `resolve_kind_p32` as a no-op compatibility stub.** Rejected: there's one in-tree consumer of the SDK, and ADR-0028 already set the precedent of "no compat shim for in-tree-only changes pre-1.0."
- **Merge with kind hashing into a single ADR/PR.** Rejected: ADR-0029's 2-PR split (widen → hash) was readable and each PR landed cleanly. Mirror the same shape here.
