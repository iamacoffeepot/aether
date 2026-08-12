# Driving a bloom over the REST control API

The Bloomery coordinator ships a REST control ingress (ADR-0149 §Packaging):
a native `BloomeryApiCapability` router mounted on the `aether.http.server`
capability, so an operator drives a bloom's whole lifecycle — stage
workpieces, shape and seal a draft, supersede, and read the live blooms /
view document / journal / artifacts — from `curl`, with no typed-mail RPC
vocabulary. The RPC ingress stays mounted alongside it for fleet plumbing;
this API is the human/shell surface.

## Booting the coordinator with the API

The HTTP ingress binds `AETHER_HTTP_PORT` (default `8910`) on localhost. The
write and live-read routes need the control-core reducer, which is a native
capability the chassis boots for you — there is nothing to point it at:

```bash
AETHER_HTTP_PORT=8910 \
AETHER_STORE_PATH=bloomery.db \
  cargo run -p aether-chassis-bloomery --bin bloomery
```

The ingress is an internal, unversioned, localhost-only control surface — no
auth (a versioned public protocol is a later arc). The read routes that hit
the store (`/journal`) and artifacts (`/artifacts/{digest}`) answer from those
capabilities alone; the seal / supersede / live-read routes (`/blooms`,
`/view`) go through the control core.

Sealing consults a tier policy, and the draft chooses which one. A draft that
seals an `aether.bloomery.approval_policy` entry in its bloom-wide registry is
gated against that value, so the tier its members were admitted at is part of
what the bloom attests. A draft that seals none falls back to the file the
pre-seal approve gate loads at boot from `AETHER_APPROVAL_POLICY_FILE` —
`approval-policy.yml` by default, resolved against the working directory, so
launch from the repository root. The startup line `bloomery REST control api
mounted policy_loaded=true` confirms the gate has that fallback. Without it, a
draft sealing no policy of its own has nothing to be decided over and its seal is
refused with `approval policy unavailable; seal fails closed`.

## The route table

| Method & path | Effect |
|---|---|
| `POST /workpieces` | Stage a workpiece (in-memory, pre-seal shaping). |
| `GET /workpieces` | List staged workpieces. |
| `POST /drafts` | Open an empty draft; returns its handle (`draft_id`). |
| `GET /drafts` · `GET /drafts/{id}` | List / read open drafts. |
| `PATCH /drafts/{id}` | Replace the present fields of a draft (membership, base, configuration registry, forecast). |
| `POST /configs` | Canonically encode and durably store a configuration by kind; returns its content address. |
| `POST /drafts/{id}/seal` | Run the approve gate over every proposal (the body carries one scope projection per member), freeze the draft to a `BloomSpec`, and admit `Fact::Seal`; returns the reducer outcome. |
| `POST /blooms/{id}/supersede` | Seal the named successor draft and admit `Fact::Supersede` against the `{id}` predecessor. |
| `POST /blooms/{id}/grant` | Hand a wedged member more attempts and resume it on the `{id}` bloom, without sealing anything. |
| `POST /blooms/{id}/answer` | Adopt an owner-signed answer statement to a parked question, releasing the hold it took. |
| `GET /blooms` · `GET /view` | The whole live view document. |
| `GET /blooms/{id}` | One bloom's live view (`{id}` is the bloom's hex digest). |
| `GET /journal` | The whole journal, decoded, oldest first. |
| `GET /artifacts/{digest}` | The content-addressed artifact bytes, or `404`. |

Request and response bodies are JSON over the `aether-bloomery` value types
(`Workpiece`, `BloomDraft`, `Membership`, `ViewDocument`, …) via serde. Three
representation notes carry from those types:

- **Digests** (a workpiece intent, a bloom's `base`, a `BloomId`) serialize as
  a 32-element byte array in request and response bodies — serde's native form
  for the `Digest([u8; 32])` newtype.
- **Bloom ids in a URL path** (`/blooms/{id}`) are the lowercase **hex** of that
  digest. The seal outcome hands the id back as the byte array, so hex-encode
  it before addressing the bloom by path.
- **Configuration registries** map a kind name to the content address returned
  by `POST /configs`. A draft with no stage-catalog entry uses the compiled
  default line; an entry names the authored catalog the bloom runs. The same
  registry carries the approval policy under `aether.bloomery.approval_policy`,
  and that one is **bloom-wide only**: a member sealing its own entry would pick
  the tier deciding whether that member may be admitted, so the seal is refused
  outright rather than resolving or ignoring it. A present entry whose kind or
  content the host cannot resolve fails loudly rather than silently falling back.

## A curl walkthrough

The walkthrough reuses a handful of digests, so bind them once — three
placeholder byte arrays. It authors its stage catalog through the same generic
route every configuration uses:

```bash
intent='[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2]'
revision='[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]'
detail='[9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9]'
base='[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1]'
```

Stage a workpiece (its `intent` / `scope_revision` are digest byte arrays):

```bash
curl -s -X POST localhost:8910/workpieces \
  -H 'content-type: application/json' \
  -d "{\"id\":\"wp-1\",\"intent\":$intent,\"scope_revision\":$revision}"
```

Open a draft and read the handle it mints:

```bash
curl -s -X POST localhost:8910/drafts        # → {"draft_id":"1","draft":{…}}
```

Author a stage catalog, then shape the draft into an admissible bloom. The
catalog below is illustrative; it must bind every stage exactly once and name
only host-routable processes:

```bash
catalog=$(curl -s -X POST localhost:8910/configs -H 'content-type: application/json' -d @catalog.json | jq '.digest')
```

`catalog.json` has the generic authoring shape
`{"kind":"aether.bloomery.stage_catalog","value":{...}}`. `value` is the
full `StageCatalog` JSON document; save the returned `digest` under the same
kind in the draft registry.

```bash
curl -s -X PATCH localhost:8910/drafts/1 -H 'content-type: application/json' -d @- <<JSON
{
  "proposals": [
    {
      "workpiece": "wp-1",
      "scope_revision": $revision,
      "configs": { "entries": {} },
      "approval": { "subject": $revision, "kind": "Approval", "detail": $detail }
    }
  ],
  "base": $base,
  "configs": { "entries": { "aether.bloomery.stage_catalog": $catalog } }
}
JSON
```

Omit `configs` to use the compiled default stage line. A partial `PATCH`
preserves the existing registry, while a present `configs` object replaces it.

The `approval` here is a placeholder that only has to be reducer-shaped
(an `Approval` binding the member's own `scope_revision`) — the seal replaces it
with the approval its gate forms, so the sealed bloom carries a policy-authored
approval rather than the operator's assertion.

Seal it. The body carries one **scope projection** per proposal, which is what
the gate decides over; the outcome names the sealed bloom id:

```bash
curl -s -X POST localhost:8910/drafts/1/seal -H 'content-type: application/json' -d @- <<JSON
{
  "projections": [
    {
      "workpiece": "wp-1",
      "scope_revision": $revision,
      "declared_surface": ["docs/guide/recipes/bloomery-rest-api.md"],
      "completeness": {
        "has_problem_statement": true,
        "has_design_notes": true,
        "has_implementation_plan": true,
        "referenced_adr_prs_merged": true,
        "model_routing_count": 1,
        "blocked": false,
        "declared_surface_fresh": true,
        "dependencies_all_closed": true,
        "umbrella_integrity": true
      },
      "adr_touch": "None",
      "pre_approved": false
    }
  ],
  "descriptions": { "wp-1": "Correct the seal walkthrough." }
}
JSON
# → {"outcome":{"Sealed":[19,165,76,49,…]}}
```

A projection is matched to its proposal by `{workpiece, scope_revision}`, and
both halves have to equal the proposal's exactly. The rest of the entry is the
evidence the gate rules on:

- `declared_surface` — the paths the change touches. The tier policy the draft
  resolves (its sealed entry, else the file fallback) resolves them
  most-restrictive-match-wins; an `auto` surface (`docs/guide/**` among them in
  the shipped file) lets the gate form the approval itself.
- `completeness` — the nine facts the gate fails closed on. Every boolean must
  hold, `blocked` must be false, and `model_routing_count` must be exactly `1`.
- `adr_touch` — `"None"`, `"ProposedOnly"`, or `"NewOrEstablished"`. The last
  routes to the owner unconditionally, ahead of any policy lookup.
- `pre_approved` — an owner-verified override that waives the tier to `auto`
  and none of the checks above.
- `signed_statement` — the owner-signed statement an above-`auto` member needs.
  The seal defers on an `aether.signing` verification of it and admits only once
  every such member verifies.

The gate fails closed at every branch, so a `422` names what fell short —
`member wp-1 has no scope projection; seal fails closed` for a proposal the
`projections` list does not cover, and a seal carrying an empty list refuses any
draft that has members.

### Sealing the tier policy

To have the bloom carry the policy it was admitted under rather than inherit the
coordinator's file, author one through the same generic route the stage catalog
uses and name it bloom-wide:

```bash
policy=$(curl -s -X POST localhost:8910/configs \
  -H 'content-type: application/json' \
  -d '{"kind":"aether.bloomery.approval_policy","value":{"default":"Judge","rules":[{"glob":"docs/guide/**","tier":"Auto"}]}}' | jq '.digest')

curl -s -X PATCH localhost:8910/drafts/1 -H 'content-type: application/json' \
  -d "{\"configs\":{\"entries\":{\"aether.bloomery.approval_policy\":$policy}}}"
```

`default` and each rule's `tier` are `"Auto"`, `"Judge"`, or `"Human"` — the JSON
spelling of the tier enum, not the lowercase words the fallback file uses. Rules
are unordered; resolution is most-restrictive-wins over the declared surface, the
same semantics the file has.

Four ways a seal is refused on this axis, all `422` and all before any admit:

| Response | Cause |
|---|---|
| `member wp-1 seals its own approval policy; the tier policy is bloom-wide only, seal fails closed` | The entry sits in a member's registry rather than the draft's. |
| `sealed approval policy unresolvable: …` | The bloom-wide address names content that is missing, filed under another kind, or no longer decodes. |
| `configuration set not yet read; seal fails closed` | The seal arrived before the coordinator finished its boot configuration read. Retry. |
| `approval policy unavailable; seal fails closed` | The draft seals no policy and the fallback file did not load. |

`descriptions` is the one advisory field on the body: per-member work-order
text, keyed by workpiece id, which the construct lane's prompt names as its
`## Task`. A member absent from the map dispatches without one, and never
blocks the seal.

Read the sealed bloom back — the whole view document, then one bloom by its
hex id:

```bash
curl -s localhost:8910/view                    # → {"mainline":[…],"blooms":[{…}]}
curl -s localhost:8910/blooms/<hex-of-the-32-bytes>
```

Read the journal (the seal is now a durable record) and fetch a referenced
artifact by its digest:

```bash
curl -s localhost:8910/journal                 # → {"records":[{"sequence":1,"idempotency_key":"…","event":{…}}]}
curl -s localhost:8910/artifacts/<digest>      # → the raw bytes, or 404
```

To supersede, seal the successor draft in the same call — the predecessor is
the path id:

```bash
curl -s -X POST localhost:8910/blooms/<predecessor-hex>/supersede \
  -H 'content-type: application/json' \
  -d '{"successor_draft":"2"}'                 # → {"outcome":{"Superseded":{…}}}
```

Supersession seals the successor from the draft as it stands, on the approval
evidence that draft already carries, so its body takes no projections.

A member that wedged because its *environment* broke — a sandbox that could run
nothing, a disk that filled — has nothing wrong with its sealed work, so
superseding it would mean altering a field of the spec to say so and discarding
the candidate it had already built. Grant it attempts on the bloom it already
belongs to instead:

```bash
curl -s -X POST localhost:8910/blooms/<bloom-hex>/grant \
  -H 'content-type: application/json' \
  -d '{"workpiece":"4708","stage":"Verify","attempts":2}'
                                               # → {"outcome":{"AttemptsGranted":{…}}}
```

`attempts` is how many more dispatched attempts the member may spend before it
wedges again, bounded by the stage's own retry budget in the sealed stage
catalog — the whole retry authority, with no second bloom-wide cap layered over
it. A `Verify` grant resumes the member at `Refine`, since re-running the
mechanical gate on an unchanged candidate cannot change its verdict.

The sealed `base` is what divides the two verbs. A base that has not moved, with
scope, membership, and configuration unchanged, is an execution decision — a
grant. A moved base, or changed scope, membership, or configuration, is a
successor doing real work — a supersession.

## How it works

The router claims a small set of path prefixes on the HTTP server cap and
dispatches every request through one handler that switches on method + path.
The in-memory shaping routes (workpieces, drafts) answer synchronously; the
durable routes forward a mail to a peer cap — the control core
(`aether.bloomery.admit` / `aether.bloomery.query`), the store
(`aether.store.replay_journal`), or the artifacts cap (`aether.artifacts.get`)
— and answer the HTTP client only when that reply lands, correlating the
deferred reply the same way the RPC and HTTP server caps do.
