# ADR-0044: Component capabilities via wasm custom section

- **Status:** Proposed (parked)
- **Date:** 2026-04-24
- **Parked:** 2026-04-24 — the design is useful framework for a future revisit, not active near-term work. The ADR-0043 net sink keeps `AETHER_NET_ALLOWLIST` as its permissioning layer in the interim; this ADR is what we pick back up when a concrete forcing function shows up (multi-component chassis with mixed-trust wasm, third-party component loading, a loud-enough threat model, or the second sink that wants per-component scoping).

## Context

ADR-0043 shipped the net sink with `AETHER_NET_ALLOWLIST` as the permissioning stopgap — a chassis-wide env var, deny-by-default, enforced at dispatch. The ADR was explicit that this was the crude v1 and a proper capabilities system would follow. That follow-up is this ADR.

Two problems with the stopgap, both structural:

- **Chassis-wide.** Every component on a substrate shares one allowlist. A component that needs `api.openai.com` and another that needs `github.com` collapse into a union — each gets both. No per-component scoping means no least-privilege; a compromised or misbehaving component reaches every allowed host.
- **Invisible.** The operator (and Claude-in-harness) can't see what hosts a component wants to contact by inspecting the wasm. `describe_component` reports handlers but not outbound reach. The question "what external services is this component wired to talk to?" has no structured answer short of reading source.

The same pattern will recur as soon as a second sink wants scoped permissioning — the io sink (ADR-0041) grants every component equal reach into every namespace; a component that should only touch `save://` today can freely write `config://`. Audio, render, camera are blunt-grant-or-nothing (no useful sub-scoping), but should still be declarable so the operator can see which components touch them. Every sink that ships after this ADR inherits the same shape.

The aether codebase already uses wasm custom sections for per-component metadata the substrate and hub read at load without executing the wasm: `aether.kinds` (ADR-0028, ADR-0032) for the kind manifest, `aether.kinds.labels` (ADR-0032) for canonical-bytes labels, `aether.kinds.inputs` (ADR-0033) for the handler-driven inputs manifest. The pattern is mature — emit a `#[used] #[link_section = "aether.<name>"]` static from an SDK macro, hub parses at `load_component`, MCP surfaces it, the substrate enforces. Capabilities slot in cleanly alongside these.

This ADR decides: wire format (`aether.caps` custom section, postcard-encoded manifest), capability shape (per-sink typed scopes), enforcement point (substrate checks at mail dispatch), load-time surface (hub reads and grants; MCP exposes via `describe_component`), guest SDK (`capabilities!` macro), error propagation (new `CapabilityDenied` variants on sink error enums), and the migration path from `AETHER_NET_ALLOWLIST`.

### Framing: mail-layer packet firewall

The enforcement model is a firewall applied to mail frames. The standard L3/L4 vs L7 split maps cleanly onto our phasing:

- **L3/L4-style (routing-only).** Gate on the tuple `(sender_mailbox, recipient_mailbox, kind_id)`. Answers "who is allowed to send what kind to which recipient." Ignores payload.
- **L7-style (content-scoped).** Decode the kind's payload, inspect specific fields, match against grant scope. Answers "what is actually in the message." Per-sink checker logic — net reads the URL host; io reads the namespace + operation flag.

L7 is a superset of L3/L4: a `CapScope::All` entry in the manifest is the routing-only grant inside the content-scoped framework, and an author who wants per-host gating declares `CapScope::NetHost(...)` instead. The same wire format carries both, so phasing enforcement — L3/L4 first, L7 later — moves no bytes on the wire.

The mail layer is a cleaner DPI surface than TCP because the payload schema is known at decode time (no parsing ambiguity), there's no fragmentation to reassemble, and there's no encryption to MITM. The "cost" DPI typically carries (CPU per packet, false positives on fragmented/encrypted streams) doesn't apply here. The substrate already decodes the incoming kind on the dispatch path to hand fields to the adapter — net decodes `Fetch` to pull the URL for ureq, io decodes `Read`/`Write`/`Delete`/`List` to pull the namespace for adapter lookup — so the cap check is one more field read in an already-live decode, not a new parse.

This framing also makes the audit follow-up obvious: denied mail gets logged the same way `iptables -j LOG` records dropped packets, feeding forensic review and policy refinement.

## Decision

### 1. Custom section: `aether.caps` v0x01

Each component emits a single custom section named `aether.caps` carrying a postcard-encoded `CapsManifest`:

```rust
struct CapsManifest {
    version: u8,           // 0x01 for this ADR; future changes bump.
    caps: Vec<Capability>,
}

struct Capability {
    sink: String,          // Short sink name: "net", "io", "audio", ...
    scope: CapScope,
}

enum CapScope {
    /// Blanket grant. The component may send any kind to this sink.
    /// Used for sinks where sub-scoping isn't useful (render, camera,
    /// audio) or where the caller needs full access.
    All,

    /// Net: one hostname, exact match. Multiple hosts require
    /// multiple Capability entries. Wildcards (`*.github.com`) are
    /// deliberately deferred — keeps v1 enforcement literal.
    NetHost(String),

    /// Io: one namespace with read/write flags. Component that wants
    /// both read and write on `save://` declares
    /// `IoNamespace { namespace: "save", read: true, write: true }`.
    IoNamespace { namespace: String, read: bool, write: bool },
}
```

Per-record versioning follows ADR-0028/0033 precedent. The section is emitted by a `#[used] #[link_section = "aether.caps"]` static the guest SDK macro materialises. One section per component; multiple declarations in source concatenate into the single `CapsManifest::caps` vec at codegen time, not into multiple sections.

Scope variants are typed rather than stringly (e.g. `"net:api.openai.com"`) so each sink's dispatcher does a typed check rather than parsing a mini-DSL. New sinks extend `CapScope` with new variants; stringly scopes could stay if a sink genuinely doesn't fit the enum, but no forcing function today needs the escape hatch.

### 2. Enforcement: substrate checks at mail dispatch

The substrate stores granted capabilities per component in the registry, keyed by `MailboxId`. The storage is populated at `load_component` (see §4) and cleared when the component drops.

At every mail dispatch on a substrate-owned sink, the sink handler consults the granted caps for the sender's mailbox:

- **Component-origin mail** (`ReplyTo::Component(id)`): check that the component's granted caps cover the send. If no matching `Capability` covers the operation, reply with the sink's `CapabilityDenied` error variant (§3) and skip the adapter call.
- **Session-origin mail** (`ReplyTo::Session(token)`): caps don't apply. Claude-in-harness is the operator; it has full reach by definition.
- **Substrate-origin mail** (control plane, sink-to-sink): caps don't apply. Trusted native code.

The check is per-sink because scope semantics differ: the net sink checks `url.host() ∈ granted_hosts`; the io sink checks `namespace ∈ granted_namespaces` with the right `read`/`write` flag for the operation; audio/render/camera check `CapScope::All` is present. Each sink's handler owns its check — a `pub fn check_cap(granted: &[Capability], request: &Fetch) -> bool` helper per sink lives in the sink's module.

Scopes within one sink are additive (union). A component that declares two `NetHost` entries can reach both hosts; declaring `("net", NetHost("a"))` and `("net", NetHost("b"))` equals "hosts a and b."

### 3. Error propagation: `CapabilityDenied` on sink error enums

Each sink's error enum grows one variant:

```rust
pub enum NetError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,       // DEPRECATED — see §7 migration
    CapabilityDenied,      // new
    Disabled,
    AdapterError(String),
}

pub enum IoError {
    NotFound,
    Forbidden,
    UnknownNamespace,
    CapabilityDenied,      // new
    AdapterError(String),
}
```

`CapabilityDenied` is distinct from the pre-existing `Forbidden` (io) and `AllowlistDenied` (net). `Forbidden` means the adapter reached the resource and the ACL said no (e.g. writing to a read-only namespace); `AllowlistDenied` is the chassis-wide env-var stopgap; `CapabilityDenied` is the per-component cap check failing. Three distinct failure modes with three distinct names keeps the taxonomy diagnosable.

Once migration completes (§7), `AllowlistDenied` retires and `CapabilityDenied` is the sole "refused for permission reasons" variant.

### 4. Load-time flow

`load_component` today reads `aether.kinds`, `aether.kinds.labels`, and `aether.kinds.inputs` from the wasm before handshaking the component into the registry. This ADR adds `aether.caps` to that parse:

1. Hub-side or chassis-side (wherever the wasm bytes are first read) parse the `aether.caps` section. Unknown version → reject the load with a structured error. Unknown sink name in a `Capability` entry → reject with "component requests capability for unknown sink `<x>`." Malformed postcard → reject with decode error.
2. The parsed `CapsManifest` rides on the `LoadComponent` mail to the substrate, same path as `aether.kinds` metadata today.
3. Substrate stores the granted caps keyed by the newly-allocated `MailboxId`. In v1, **granted = declared**: every requested capability is granted as-is. The operator approval / narrow flow is deferred (§7 follow-up).
4. If the wasm has no `aether.caps` section, the component gets an empty grant set. A warn fires at load time: `component loaded with no capabilities — substrate-owned sinks will refuse its mail`. Not an error — a component that only talks to other components stays useful. The warn exists so a component that was *meant* to declare caps but forgot gets a loud signal before its first fetch returns `CapabilityDenied`.

### 5. MCP `describe_component` extension

The hub's `describe_component(engine_id, mailbox_id)` response grows a `capabilities` field:

```json
{
  "name": "asset_pipeline",
  "doc": "...",
  "receives": [ ... ],
  "fallback": null,
  "capabilities": [
    { "sink": "net", "scope": { "NetHost": "api.openai.com" } },
    { "sink": "io",  "scope": { "IoNamespace": { "namespace": "save", "read": true, "write": true } } }
  ]
}
```

Claude (and any human inspecting the harness) answers "what is this component wired to reach?" from wasm metadata, without running it. The structural answer feeds both auditing (is this component doing what the author said it does?) and tooling (a dry-run "what would this component touch?" reporter falls out).

### 6. Guest SDK: `capabilities!` macro

Component authors declare caps with a module-scope declarative macro in the SDK:

```rust
use aether_component::capabilities;

capabilities! {
    net: "api.openai.com",
    net: "httpbin.org",
    io: save(rw),
    io: assets(r),
    audio,
    render,
}
```

The macro expands to a `#[used] #[link_section = "aether.caps"]` static holding the postcard-encoded manifest. Same pattern as `#[handlers]` for the inputs manifest — declaration is the only source of truth, the custom section is codegen, no parallel list.

Per-scope syntax: `net: "<host>"` for `NetHost`, `io: <ns>(r|w|rw)` for `IoNamespace`, bare sink name (`audio`, `render`, `camera`) for `CapScope::All`. The macro owns the mapping from ergonomic syntax to typed enum variants; adding a new sink means extending the macro (and the `CapScope` enum) in one place.

Calling `capabilities!` more than once in the same crate is a compile error (ambiguous: which one is the manifest?). Components that need conditional caps use `cfg` attributes inside a single macro invocation, not multiple invocations.

### 7. Migration from `AETHER_NET_ALLOWLIST`

Phased to avoid breaking any shipped component in one go:

1. **Phase A — coexist.** Net sink checks *both* the env allowlist and the cap check; either grants access. `AllowlistDenied` stays the error when the env allowlist denies; `CapabilityDenied` is the new error when the cap check denies. v1 of this ADR ships Phase A.
2. **Phase B — default off.** `AETHER_NET_ALLOWLIST` stops granting by default. A new var `AETHER_NET_LEGACY_ALLOWLIST=1` opts a chassis back into env-allowlist behaviour for dev/test convenience. The per-component declarations are authoritative.
3. **Phase C — retire.** `AETHER_NET_ALLOWLIST` removed entirely. Dev/test convenience moves to `AETHER_CAPS_GRANT_ALL=1` (grants every substrate sink to every component, no cap checks run). Production leaves it unset.

The phase timing is loose — Phase B follows once every shipped component (reference + demos) carries `capabilities!` declarations; Phase C follows once the guest SDK migration has soaked for a release cycle.

### 8. Phasing

Enforcement splits into two phases; the wire format, macro surface, hub-side parse, and MCP exposure all ship complete in Phase 1 so component authors can write precise `capabilities!` declarations from day one. Only the substrate's enforcement depth changes between phases, which means a component author's declaration tightens enforcement around itself automatically as Phase 2 lands — no wire migration, no SDK re-release, no author churn.

**Phase 1 — routing-only enforcement (L3/L4).**
- Wire format: `aether.caps` section, postcard manifest, versioned. Full `CapScope` enum defined on the wire.
- `Capability` / `CapScope` in `aether-kinds` (or a new `aether-caps` crate).
- Substrate-side storage: per-mailbox grant map in the registry.
- Enforcement: substrate checks that the sender has *any* `Capability` entry naming the target sink. Scope field is parsed and surfaced but not matched against the request.
- `CapabilityDenied` variants on `NetError` and `IoError` — triggered by "no cap for this sink," not yet by scope mismatch.
- `capabilities!` macro in `aether-component` — full surface, components declare scopes precisely even though they aren't yet enforced.
- `describe_component` extension in the hub — scopes surface to MCP immediately (visibility is part of the value).
- Migration Phase A — coexist with `AETHER_NET_ALLOWLIST`. Host scoping continues to flow through the env var during this phase.

**Phase 2 — content-scoped enforcement (L7).**
- Per-sink cap checkers for `net` (URL host extraction + match) and `io` (namespace match + read/write flag check against operation).
- Scope field is now authoritative: `NetHost("api.openai.com")` rejects fetches to other hosts; `IoNamespace { save, read: true, write: false }` rejects writes to `save://`.
- Migration Phase B — `AETHER_NET_ALLOWLIST` stops granting by default; per-component declarations are authoritative. `AETHER_NET_LEGACY_ALLOWLIST=1` is the dev/test opt-in.
- Migration Phase C — `AETHER_NET_ALLOWLIST` retired entirely; `AETHER_CAPS_GRANT_ALL=1` is the dev/test escape hatch for "no enforcement."

Audit-log hookup for denied mail (L3/L4 and L7) rides alongside Phase 2 as the natural forensic layer — parked until Phase 2 itself is in flight.

**Deferred beyond both phases:**
- Operator grant/deny/narrow flow at `load_component`. Both phases grant exactly what the wasm declared. The MCP surface to "load but only grant a subset" is a follow-up once we know what shape the operator workflow wants.
- Wildcard matching (`*.github.com`, `https://api.*/v1/...`).
- Sub-scoping on `aether.control` (e.g. cap for `subscribe_input` but not `load_component`).
- Dynamic cap changes (a component requesting additional caps at runtime).
- Cross-component mail gating (component A mails component B's mailbox — currently unrestricted in both phases).
- Capability revocation mid-session.

## Consequences

### Positive

- **Least privilege by default.** A component only reaches the sinks and scopes it declared. The blast radius of a compromised or buggy component is bounded by its manifest.
- **Structural visibility.** `describe_component` answers "what does this component touch?" from wasm metadata, without running the component. Auditing and tool-chain integration get a real API instead of "grep source."
- **Pattern reuse.** `aether.caps` is the fourth custom section after `aether.kinds`, `aether.kinds.labels`, `aether.kinds.inputs`. The emit/parse/surface machinery is the same; adding more sections stays a one-afternoon PR.
- **Typed SDK.** `capabilities! { net: "foo.com", io: save(rw) }` — ergonomic declaration, typed enum under the hood, compile-time rejection of bad sink names. Matches the `#[handlers]` / `Sink<K>` ergonomics the SDK has converged on.
- **Forward compatibility.** Version-bytecode on the section. Adding new scope variants (wildcards, method-scoping for net) bumps the version, and a hub that understands a higher version can still read a lower one.

### Negative

- **Migration cost.** Every component that currently touches substrate-owned sinks needs a `capabilities!` declaration. Existing components (camera, player, hello, save_counter, demos) need updates before Phase B; each is a one-line-ish change, but the diff fans out.
- **Error variant growth.** `NetError` and `IoError` grow `CapabilityDenied`; `NetError` temporarily carries both `AllowlistDenied` and `CapabilityDenied` during Phase A. Each error enum is still small, but the "different reasons for refusal" taxonomy is bigger than "just say no."
- **Macro churn.** The `capabilities!` macro lives near `#[handlers]` in the SDK complexity budget. ~300 lines of proc-macro work including validation — real maintenance cost, mitigated by covering it with tests the same way `#[handlers]` is tested.
- **V1 doesn't gate the load itself.** A component declaring `net: "api.openai.com"` gets that grant automatically. There's no "operator reviews and approves at load" step in v1, so a component can bake in whatever capability it wants and have it granted. This is strictly no worse than the current `AETHER_NET_ALLOWLIST` world (which blanket-grants to every component regardless), but it isn't the full operator-in-the-loop shape that "capabilities" connotes in, say, a hardened OS. That shape is the follow-up.

### Neutral

- **No new host fn.** Enforcement is substrate-side, no FFI surface change. Components don't know they're being gated — they just see the reply kind come back `Err`.
- **Component-to-component mail stays unrestricted in v1.** Sinks are substrate-owned; mail between two components doesn't go through any sink check. Adding that gate later is possible (mail path sees both sender and recipient ids) but isn't in scope.
- **Deny-by-default semantics match the ADR-0043 stopgap.** A component with no caps declaration gets nothing. The failure mode is identical to forgetting the env var today, just localised to one component.
- **Wire-format-neutral.** Postcard matches every other custom section. Hub decode code reuses the existing schema-manifest helpers.

## Alternatives considered

- **Grant all, deny via blocklist.** Every component gets universal access, but the operator can declare deny rules. Rejected: defeats the default-secure stance, and the operator UX of "notice and block" is strictly worse than "grant on declaration."
- **Capabilities passed at load time, not declared in the wasm.** The operator specifies at `load_component`: "load this wasm, grant it net+api.openai.com." Rejected as the *only* source: the component author knows what their component needs; making the operator re-derive it from docs every time is fragile. But this shape *is* the deferred operator narrow flow — the declaration in the wasm is the request; the operator's grant is the truth. V1 collapses them (grant = declaration), follow-up separates them.
- **Stringly-typed scopes per sink.** `Capability { sink: "net", scope: "api.openai.com" }` — each sink parses its own mini-DSL. Rejected: typed enum is more constrained and the SDK macro has less mapping work to do. Reopen if a sink's scope genuinely doesn't fit enum variants.
- **Capability handles (unforgeable tokens).** Grant the component a token at load time; host fn sends include the token; substrate validates per send. Rejected: brings object-capability semantics the aether mail surface doesn't need — no sharing/delegation of caps, no revocation, no composition. The declared-at-load-time model matches the rest of the SDK.
- **Extend `aether.kinds.inputs` with a capabilities field.** Rejected on the same grounds ADR-0033 split inputs from `aether.kinds`: "what a component handles" and "what a component can send" are semantically distinct. Two sections with two readers is cleaner than one section with two purposes.
- **Runtime capability promotion (component asks for a cap at runtime).** Rejected for v1: adds a new mail shape (`aether.caps.request_cap`), needs operator-in-the-loop approval, and isn't needed by any current forcing function. The asset pipeline and the GH-integration cases both know their caps at component-build time.
- **Separate `aether-caps` crate vs folding into `aether-kinds`.** Leaning toward a new `aether-caps` crate so the dependency direction stays clean (both the substrate and hub import it, but the SDK macro is the only emitter). Folding into `aether-kinds` is workable but makes `aether-kinds` carry permissioning concerns it otherwise wouldn't. Decision: new crate; reopens during PR if dep-graph analysis suggests folding is cheaper.

## Follow-up work

Parked as of 2026-04-24 — nothing below is actively scheduled. Listed in the rough order they'd be picked up once the ADR unparks.

**Phase 1 (routing-only enforcement):**
- Define `aether-caps` crate with `CapsManifest`, `Capability`, `CapScope`, and wire-format roundtrip tests.
- Hub-side parser — read `aether.caps` during `load_component`, reject unknown versions / sinks, include in `LoadComponent` mail to the substrate.
- Substrate-side storage — per-mailbox grant map; routing check at mail dispatch for substrate-owned sinks.
- `NetError::CapabilityDenied` + `IoError::CapabilityDenied`. Phase A migration: env allowlist OR cap check grants access.
- `capabilities!` macro in `aether-component`, with proc-macro validation (unknown sink name → compile error, duplicate invocation → compile error). Full scope surface — authors declare `NetHost(...)` / `IoNamespace { .. }` even though Phase 1 enforcement ignores it.
- `describe_component` extension in `aether-substrate-hub` — surface parsed caps (including scopes) to MCP for operator visibility.
- Migrate existing components (camera, player, hello, save_counter, demos/sokoban, demos/tic-tac-toe) to declare `capabilities!`.

**Phase 2 (content-scoped enforcement):**
- Per-sink cap checkers: net (`url::Url` host extraction + match), io (namespace match + read/write flag check against operation). Checkers live in the sink module next to the adapter they gate.
- Migration Phase B — `AETHER_NET_ALLOWLIST` off by default; `AETHER_NET_LEGACY_ALLOWLIST=1` opts a chassis into env-driven behaviour for dev.
- Migration Phase C — `AETHER_NET_ALLOWLIST` removed entirely; `AETHER_CAPS_GRANT_ALL=1` is the dev/test escape.
- Audit log of cap-denied attempts (mail-layer equivalent of `iptables -j LOG`). Feeds security review and policy refinement.

**Deferred beyond Phase 2:**
- Operator grant/deny/narrow flow at `load_component`. Optional `granted_caps` override on the load tool; hub surfaces declared-vs-granted diff.
- Wildcard scoping (`*.github.com`, method-scoping for net). Needs a concrete forcing function before designing the matcher.
- Cross-component cap gating. Needs a threat model for component-attacks-component that doesn't exist yet.
- Capability revocation mid-session.
