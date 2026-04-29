# ADR-0064: Type-tagged opaque ids on the MCP wire

- **Status:** Proposed
- **Date:** 2026-04-28

## Context

ADR-0029 and ADR-0030 made every mailbox id and every kind id a 64-bit
FNV-1a hash. Names and schemas hash deterministically; the resulting
`u64` is the address used by the host fns, the scheduler, and the hub
wire. Disjoint domains (`MAILBOX_DOMAIN` / `KIND_DOMAIN`) prefix the
input bytes so the two id spaces don't overlap in practice.

Three problems have surfaced as the MCP harness has grown:

**JSON precision loss on the wire.** MCP serialises every tool argument
as JSON. JSON's number type is an IEEE-754 double; integers above
`2^53` round to the nearest representable double on parse. Most 64-bit
hashes land above that threshold, so a mailbox id that round-trips
through MCP loses its low-order bits. `mcp__aether-hub__replace_component`
was the first surface bitten — the agent reads a `mailbox_id` from a
`load_component` reply, passes it back, and the hub looks up the
rounded value in the registry and reports "no such mailbox." The
workflow today is to terminate-and-respawn rather than trust the id
round-trip.

**No type guard at the call site.** A `u64` is a `u64`. Passing a kind
id where a mailbox id is expected is a silent error inside the
substrate (registry returns `None`, mail bubble-drops, agent gets a
quiet warning). The byte-domain prefixes from ADR-0029/0030 make the
hash spaces *practically* disjoint (no realistic collision between a
mailbox-domain hash and a kind-domain hash), but the `u64` type itself
carries no marker that says which space it belongs to. A copy-paste
slip is invisible until the runtime fails.

**Opaque ids look like quantities.** When an agent reads
`mailbox_id: 7820342551122739012` from a tool reply, there's no
visual cue that this is an opaque token, not an integer. It can be
arithmetically manipulated, compared with `<`, formatted with
thousands-separators — none of which is meaningful. The MCP surface
treats agents as a first-class caller; reading the same field as
`mbx-...` would prevent half a class of confusions before they
happened.

ADR-0063 named "u64 ids over MCP" as one of the short-distance fixes
that an eventual failover ADR will lean on. This is that fix.

## Decision

Re-shape every `u64` id space (mailbox, kind, reply-handle) so it
carries a 4-bit type tag in its high bits and a 60-bit hash in its
low bits, and define a deterministic string encoding for the wire.

**Bit layout.**

```
bit  63                                                            0
     ┌──────┬──────────────────────────────────────────────────────┐
     │ tag  │                       hash                           │
     │  4b  │                       60b                            │
     └──────┴──────────────────────────────────────────────────────┘
```

The tag identifies the id space. The hash is the low 60 bits of
`fnv1a_64(domain ++ name)` (or schema, for kinds), masked with
`0x0FFF_FFFF_FFFF_FFFF`.

**Tag assignments (v1).**

| Tag (hex) | Prefix | Space               |
|-----------|--------|---------------------|
| `0x1`     | `mbx-` | Mailbox ids         |
| `0x2`     | `knd-` | Kind ids            |
| `0x3`     | `hdl-` | Reply-handle ids    |
| `0x0`     | —      | Reserved (rejects)  |
| `0x4..0xF`| —      | Reserved for future |

`0x0` is intentionally invalid so a zero-initialised `u64` (`0u64`)
can never be mistaken for a real id.

**String encoding.** A tagged id renders as `<prefix><body>` where
`<body>` is the 60-bit hash in lowercase base32 alphabetical
(`a..z` + `2..7`, RFC 4648 alphabet, leading-zero preserving), grouped
into three 4-character chunks with `-` separators:

```
mbx-q3lr-bv2x-mtdr   ← 12 base32 chars + 3 dashes
knd-a3f2-5b6c-d4e2
hdl-7e6f-zzzz-2345
```

60 bits ÷ 5 bits/char = 12 chars exactly — no padding, no slack.
Decoder rules: case-insensitive read, prefix → tag lookup, body
decoded to 60 bits, OR'd with `(tag << 60)`. Mismatched prefix or
malformed body returns a typed error (not `0u64`).

**Where the encoding boundary sits.** The string form is the wire
format for the MCP boundary and human-facing diagnostics
(`engine_logs`, tracing fields, error messages that mention an id).
Internal types stay `u64`. Conversion happens at the hub MCP
serialiser/deserialiser, at the tracing `Display` impl for each id
type, and nowhere else. Host fns, the scheduler, the registry,
postcard-encoded mail payloads all see raw `u64`.

**Per-type id newtypes.** `MailboxId` already exists as a newtype.
`KindId` and `HandleId` get the same treatment if they don't already.
The constructor for each newtype asserts the high tag matches at
debug-build time (release-build it's a no-op — the encoder is the
gatekeeper). A `From<u64>` impl is *not* provided; use the explicit
`MailboxId::from_tagged_u64` / `from_string` constructors.

**Const fold.** The `Kind::ID` derive and the `mailbox_id_from_name`
const fn both already run at compile time. They gain one `&` and one
`|` to mask the hash and OR the tag. Compile cost: zero.

**Migration.** No deployed ids exist outside dev sessions. Every
`Kind::ID` recomputes at the next build; every name-hashed mailbox
id recomputes the same way. Tests that hard-code an id literal need
re-baselining (the new value differs in 4 high bits). Persisted
state — `save://` files, on-disk artefacts — does not currently store
ids, so there is nothing on disk to migrate. If that changes before
this ADR ships, the migration becomes a one-shot re-hash on load.

## Consequences

**Per-type collision space (the headline number).** Each id type now
occupies its own clean 60-bit subspace. Birthday collision threshold
is `~2^30 ≈ 1.07 billion` distinct names *of the same type*. Cross-type
collisions are impossible by construction (the tag bits differ).

In practical terms:

| Id type     | Collision threshold | Realistic count    | Margin |
|-------------|---------------------|--------------------|--------|
| Mailboxes   | ~1.07 B             | thousands          | ~10⁶× |
| Kinds       | ~1.07 B             | hundreds           | ~10⁷× |
| Handles     | ~1.07 B             | thousands inflight | ~10⁶× |

For comparison, today's untagged 64-bit space gives `~2^32 ≈ 4.29 B`
per logical type (the byte-domain prefix from ADR-0029/0030 already
keeps the hashes practically disjoint). The change costs 2 bits of
birthday margin (4.29 B → 1.07 B) and gains *cryptographic* type
separation — no possible bit pattern is valid as more than one type.

**MCP wire is human-legible and round-trippable.** Agents see
`mbx-q3lr-bv2x-mtdr` in tool replies and pass that string back
verbatim. The hub's MCP layer encodes/decodes at the boundary; the
substrate sees a `u64` either way. No JSON precision loss.

**Type confusion becomes detectable.** `MailboxId::from_tagged_u64`
on a kind-tagged value returns `Err(TagMismatch)` instead of silently
constructing a junk mailbox id that the registry will later reject.
Registry-internal lookups can also assert on tag, catching
copy-paste errors at the call site instead of one mail dispatch
later.

**ADR-0029/0030 byte-domain prefixes are now a cross-check on the
tag bits.** The type is encoded in two independent places: the high
4 bits (tag) and avalanched into the low 60 bits via the
`MAILBOX_DOMAIN` / `KIND_DOMAIN` prefix fed to FNV-1a. A bit flip,
copy-paste error, or buggy reconstruction that desyncs the two —
say, a `u64` claiming `tag=0x1` (mbx) whose low 60 bits were hashed
with `KIND_DOMAIN` — is detectable by anyone who can re-derive the
hash from the original name (registry lookup, debug-build
assertions). Without the domain prefix, the low 60 bits would carry
no type information and the tag bits would be the sole authority;
flipping them would silently mint a "valid" id of the wrong type.
The two layers are self-referential: each encodes the type, and
their agreement is the integrity property.

**`engine_logs` and tracing get easier to read.** Today an
`engine_logs` line says `mailbox=7820342551122739012`. Post-this it
says `mailbox=mbx-q3lr-bv2x-mtdr`. Logs grep-friendlier, errors
unambiguous, multi-substrate sessions less confusing.

**Test churn.** Every test that hard-codes an id literal — anywhere
that asserts `mailbox_id == 0x...` or pattern-matches on a numeric
value — re-baselines. Estimated tens of test edits, mechanical. The
build will fail loudly on every site, so the migration is grep-driven
not detective work.

**Forecloses 0-tag-as-valid.** Future tag assignments cannot use
`0x0`. Current count is 3 used + 12 future slots, so this is a
constraint, not a problem.

**Forecloses ids in `>2^60` value space.** Anything that wanted the
top 4 bits for a different purpose is blocked. Nothing in v1 wants
this; flagging it for a future ADR that finds a use (e.g.
sequence-counter ids, generation-tagged handles) to reckon with the
loss.

**Doesn't change the postcard wire for mail payloads.** Cast-shape
kinds and postcard-encoded kinds still see `u64` for id fields
embedded in their payloads. The string encoding is *only* the MCP
JSON wire and human-visible diagnostics. A postcard varint stays a
postcard varint.

**Failover groundwork.** ADR-0063 listed this as one of the
prerequisites for an eventual soft-recovery / failover ADR (the
other being `Registry::drop_mailbox` on death). With this in,
agents can address a `replace_component` reliably, which is the
primitive a watchdog or auto-restart layer needs.

## Alternatives considered

**Plain prefixed-string with no bit tag** (the original direction
in the next-ADR memory note). Encode `mbx-XXXX-...` purely as a
string transformation of the raw `u64`; tag info lives only in the
prefix string, not in the bits. Rejected: the substrate-internal
type-confusion guard goes away — only the MCP boundary distinguishes
types, not the runtime. Tagged bits are strictly stronger for
modest cost (4 bits).

**JSON5 / explicit string-encoded integers.** Accept JSON5 input
where `mailbox_id: "7820342551122739012"` (string of digits) preserves
precision, and stringify on output. Rejected: keeps the
indistinguishable-from-a-number problem (string of digits *is* a
number, just quoted), adds no type guard, and forces every MCP client
to opt into JSON5. The string form here is unmistakably opaque.

**BigInt over a transport that supports it.** Wait for MCP /
target transports to grow a 64-bit integer type. Rejected on schedule
grounds — this is needed now, and the bit-tag approach is independent
of transport choice (works over JSON, JSON5, BigInt, postcard, plain
text identically).

**Larger tag (8 bits) for more headroom.** 4 bits gives 16 type
slots, of which 3 are used. 8 bits would give 256 at the cost of 4
more bits of hash margin (2^28 birthday ≈ 268 M, still 5 orders of
magnitude over realistic counts). Rejected: 16 slots is plenty for
foreseeable id types, and 60-bit hash is a rounder number for the
12-char base32 body (60 ÷ 5 = 12 exactly; 56 ÷ 5 = 11.2 needs
padding).

**Smaller tag (2 bits)** for 62-bit hash (~2^31 birthday). Rejected:
4 type slots is the same number as today (`mbx`/`knd`/`hdl`/reserved
zero) with no headroom. 4 bits costs nothing and leaves 12 spare
slots.

**Hex encoding for the body** (`mbx-a3f2-5b91-c4d0-7e8f` minus one
nibble). Rejected: 60 bits is 15 hex chars, which doesn't group
cleanly into 4-char chunks. Base32-alphabetical lands on 12 chars
exactly and avoids `0`/`O` and `1`/`l` look-alikes that hex permits.
