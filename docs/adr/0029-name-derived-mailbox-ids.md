# ADR-0029: Name-derived MailboxIds

- **Status:** Accepted (phases 1–2 shipped)
- **Date:** 2026-04-19
- **Accepted:** 2026-04-20

## Context

`MailboxId` today is a `u32` allocated by the substrate's registry as `entries.len()` at registration time — the first sink lands at `0`, the second at `1`, and component mailboxes slot in wherever they happen to register. The id is opaque and session-local: mailbox "greedy" might be id `7` in one run and id `3` in the next, depending on how many sinks and prior components registered ahead of it. Components that want to talk to a named peer call the `resolve_mailbox_p32` host fn once at init, cache the returned id, and use it for the rest of their life.

This shape carries three weights that keep recurring:

1. **Resolution round-trips.** Every component that mails a named peer pays a host-fn call up front to turn the name into an id. `Sink<K>` in the SDK wraps this, but the trip is still in the critical path of every component's init, and the mailbox must already exist when resolve runs — which forces boot ordering (sinks registered before components that resolve them).

2. **Ids are not stable across processes.** A hub today owns one substrate, so session-local ids are fine. The moment we want to coordinate across substrates — sharding components over multiple hub-supervised substrates, or letting a second Claude session address mailboxes a first session created — ids stop being the right handle. The system has to talk names over the wire and resolve at every edge, because integer ids from one substrate mean nothing in another.

3. **The registry is doing extra bookkeeping.** `entries: Vec<MailboxEntry>` + `by_name: HashMap<String, MailboxId>` + `mailbox_names: Vec<String>` exists to back the resolve call and to round-trip ids to names for observability. Most of the id→name path is cosmetic; most of the name→id path is resolve.

If the id is a deterministic function of the name, all three weights ease simultaneously. `resolve_mailbox` becomes a client-side hash — no host-fn, no ordering constraint, no cache needed. Ids carry meaning across processes: two substrates that hash the same name produce the same id, so inter-substrate routing is just "forward to whoever owns this id." The registry shrinks to a live-id set plus the sparse table that maps ids to dispatch targets — the `by_name` map and the `mailbox_names` vector both become redundant (the name is recoverable from the dispatch target's metadata when we need it for logs, or we stop caring).

**Width.** The current `u32` is too narrow for hashing: at 32 bits the birthday bound kicks in around 65k mailboxes, which sounds large until a future world with per-entity components or per-asset mailboxes lands. A 64-bit id pushes the birthday bound past 4 billion, which is the real "never collides in practice" regime. 64 bits is the standard width for this kind of content-addressed handle (Git object ids truncate to 64 for short hashes; most hash-map implementations hash to 64).

On wasm32 the natural worry is that a 64-bit id can't be passed across the FFI boundary. It can: the wasm spec supports `i64` as a host-fn parameter and return type on both wasm32 and wasm64 — ADR-0024's `_p32` suffix story is about *pointers* (which are 32-bit on wasm32 and 64-bit on wasm64), not scalars. A 64-bit `MailboxId` passed as `i64` through `send_mail` / `reply_mail` / etc. works today on wasm32 without introducing any of the dual-target complexity ADR-0024 deferred. The suffix naming convention stays intact — scalar mailbox-id params do not get a suffix.

**Scope.** This ADR is about mailbox ids only. Kind ids (ADR-0005) are a separate decision with different tradeoffs — the registry lookup paths are different, the resolve timing is different (kind ids are resolved once at init against a fixed vocabulary the component declared via ADR-0027/0028), and the collision surface is smaller. Addressing kind-id hashing is parked for a follow-up ADR once we've lived with name-derived mailbox ids.

## Decision

**`MailboxId` becomes a `u64` computed as a stable hash of the mailbox's name. The `by_name` index and explicit `register` → id assignment are removed.**

### Id computation

- **Type:** `MailboxId(pub u64)`.
- **Hash:** a non-cryptographic 64-bit hash with a stable output for a given input across builds and platforms. Candidates: `xxh3_64`, `wyhash`, FNV-1a 64, or SipHash with a fixed key. The exact choice is a small follow-up — the constraint is only "deterministic, 64-bit, fast, reasonable distribution." SipHash with a fixed zero key is attractive because Rust's stdlib already ships it and it gives adversarial resistance at no cost, but any of the candidates meet the requirements.
- **Input:** the UTF-8 bytes of the mailbox name, unnormalized. Names are already bytestrings in the registry and on the wire (ADR-0006); there is no Unicode normalization to worry about.
- **Reserved:** `MailboxId(0)` is reserved as the "unassigned / no sender" sentinel that ADR-0011 and ADR-0017 already treat as special. The hash is rejected at registration time if it collides with 0; probability ~2⁻⁶⁴, practically zero, but the guard is cheap.

### Registry changes

- Registration takes a name and derives the id in one step: `registry.register_component("greedy")` computes `MailboxId(hash("greedy"))` and inserts into the dispatch table keyed on that id.
- Collision between two live mailboxes (name A and name B hashing to the same id) is rejected with a hard error — "mailbox id collision, rename one." Given 64-bit output and realistic mailbox counts, this is expected to be a lifetime-of-project zero-event. A loud failure beats silent misrouting.
- Name-conflict detection still runs: two components trying to register under the same name conflict exactly as they do today, because they produce the same id. The error message surfaces the name, not the id.
- `by_name: HashMap<String, MailboxId>` is removed. `mailbox_name(id)` is retained as a convenience that reads the name off the dispatch target's stored metadata (`MailboxEntry` grows a `name: String` field); it is used for logs and `describe` surfaces, not dispatch.

### SDK / host-fn changes

- `resolve_mailbox_p32` is retired from the component FFI. The SDK's `Sink<K>` construction shifts from `Sink::new(K::NAME, resolve_mailbox("name"))` to `Sink::new(K::NAME, MailboxId::from_name("name"))`, where `from_name` is a pure client-side hash with the same algorithm the substrate uses.
- `MailboxId` is a 64-bit value on the FFI: exported host fns take `i64` for mailbox parameters; the `_p32` suffix does not apply (the convention covers pointers, not scalars).
- Components keep the existing `Ctx::reply` path unchanged — sender ids arrive on `receive_p32` already derived by the substrate from the sender's mailbox name.

### Boot ordering

- Hash-based resolution lifts the "sink must exist before component resolves its name" ordering rule. A component can precompute `MailboxId::from_name("hub.claude.broadcast")` at its own init, before the broadcast sink has been registered on the substrate side, and mail to that id will be buffered or dropped per the existing unknown-id policy. Today's explicit ordering stays in the substrate boot sequence (sinks come up first) as defense-in-depth; the lifted constraint is just that components no longer have to enforce it themselves.

### Wire changes

- `aether-substrate::mail::MailboxId`: `u32` → `u64`.
- Every wire kind that carries a mailbox id (`LoadResult`, `SubscribeInput { mailbox: u32 }`, `UnsubscribeInput`, `DropComponent`, `ReplaceComponent`, `PlatformInfoResult`, …) widens its field from `u32` to `u64`.
- `aether-kinds`: the `control_plane` kinds with mailbox fields bump their `SchemaType` (u32 → u64). Under ADR-0028 this rides through the embedded manifest automatically — components that declare these kinds regenerate their manifest bytes on rebuild.
- MCP tool responses (`load_component → mailbox_id`, `receive_mail` envelope metadata) carry `u64` ids. JSON handles u64 natively; Claude sessions see a bigger number, nothing else changes.

### Out of scope

- **Kind ids.** Kind ids today are registry-assigned `u32`s (ADR-0005). Moving them to hash-derived 64-bit ids has its own tradeoffs — fixed-vocabulary resolution, smaller collision surface, tight interaction with ADR-0028's per-component manifest — and is deferred to a separate ADR.
- **Cross-hub routing.** This ADR makes the id space cross-process-portable; it does *not* define a hub-to-hub routing protocol. That lands when (and if) a second hub-supervised substrate is introduced.

## Consequences

- **One fewer host fn in the component FFI.** `resolve_mailbox_p32` goes away. `Sink<K>` construction stays the same shape at the SDK level, just uses a pure function under the hood.
- **Ids are meaningful across processes and sessions.** A mailbox id logged in one substrate run identifies the same logical mailbox in the next run, and in any other substrate hashing the same name. Observability dashboards, crash logs, and session traces become comparable over time.
- **`u32` → `u64` across the wire.** One coordinated change; sokoban demo and any external driver update their schemas (ADR-0028 carries the descriptor refresh for components).
- **Registry shrinks.** `by_name: HashMap<String, MailboxId>` is deleted; dispatch is a single `HashMap<MailboxId, MailboxEntry>` lookup. `mailbox_name(id)` reads off the entry.
- **Collision risk is theoretical but nonzero.** At 64-bit width and realistic mailbox counts (tens of thousands at most), the birthday probability is far below hardware failure rates. The loud collision error at registration time means a collision is a bug we'd actually notice, not silent misrouting.
- **Dropped names produce the same id on reuse.** Today, drop-then-re-register allocates a fresh id (entries vector grows). Under hashing, the id is identical. This is usually what we want (cross-session stability), but any cache that survives a drop event must invalidate on drop explicitly — the id alone no longer signals "different mailbox." Flagged for the `Ctx::reply` / subscriber-set code paths; current audit shows both already invalidate on drop (input subscriptions clear, sender caches are per-receive).
- **`describe_kinds` / debug tooling.** Dumps that show mailbox ids become comparable between runs; this is pure upside for debugging.
- **Breaking wire change.** Like ADR-0028, executed as a single coordinated change pre-1.0. No compat shim.

## Alternatives considered

- **Keep sequential ids, add a hub-level translation layer for cross-process routing.** Rejected: pushes complexity out of the substrate into a new component and doesn't fix the per-init resolve-round-trip weight. The simplification goal is to remove mappings, not relocate them.
- **Hash to 128 bits for "truly no collision ever."** Rejected: doubles FFI and wire cost, and 64 bits is already in the "won't happen in the lifetime of the project" regime. Git chose the same tradeoff (SHA-1/256 truncated to 64 for short ids in practice).
- **Cryptographic hash (Blake3, SHA-256 truncated).** Rejected: adversarial resistance isn't a mailbox-id requirement — mailbox names are chosen by component authors, not attacker input. Non-crypto hashes are faster and compile to fewer bytes in wasm.
- **Keep `u32` width and hash to 32 bits.** Rejected above: birthday bound around 65k is too tight for a system that plausibly grows to millions of entity-scoped mailboxes.
- **Address kind ids the same way in the same ADR.** Rejected: the two changes have different risk profiles and the substrate paths barely overlap. Bundling would make this ADR harder to review and slower to land; kind-ids get their own ADR once mailbox-id hashing has bedded in.
