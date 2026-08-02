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

## The route table

| Method & path | Effect |
|---|---|
| `POST /workpieces` | Stage a workpiece (in-memory, pre-seal shaping). |
| `GET /workpieces` | List staged workpieces. |
| `POST /drafts` | Open an empty draft; returns its handle (`draft_id`). |
| `GET /drafts` · `GET /drafts/{id}` | List / read open drafts. |
| `PATCH /drafts/{id}` | Replace the present fields of a draft (membership, base, stage catalog, toolchain, policy, budget, forecast). |
| `POST /drafts/{id}/seal` | Freeze the draft to a `BloomSpec` and admit `Fact::Seal`; returns the reducer outcome. |
| `POST /blooms/{id}/supersede` | Seal the named successor draft and admit `Fact::Supersede` against the `{id}` predecessor. |
| `GET /blooms` · `GET /view` | The whole live view document. |
| `GET /blooms/{id}` | One bloom's live view (`{id}` is the bloom's hex digest). |
| `GET /journal` | The whole journal, decoded, oldest first. |
| `GET /artifacts/{digest}` | The content-addressed artifact bytes, or `404`. |

Request and response bodies are JSON over the `aether-bloomery` value types
(`Workpiece`, `BloomDraft`, `Membership`, `ViewDocument`, …) via serde. Two
representation notes carry from those types:

- **Digests** (a workpiece intent, a bloom's `base`, a `BloomId`) serialize as
  a 32-element byte array in request and response bodies — serde's native form
  for the `Digest([u8; 32])` newtype.
- **Bloom ids in a URL path** (`/blooms/{id}`) are the lowercase **hex** of that
  digest. The seal outcome hands the id back as the byte array, so hex-encode
  it before addressing the bloom by path.

## A curl walkthrough

Stage a workpiece (its `intent` / `scope_revision` are digest byte arrays):

```bash
curl -s -X POST localhost:8910/workpieces \
  -H 'content-type: application/json' \
  -d '{"id":"wp-1","intent":[2,2,...,2],"scope_revision":[3,3,...,3]}'
```

Open a draft and read the handle it mints:

```bash
curl -s -X POST localhost:8910/drafts        # → {"draft_id":"1","draft":{…}}
```

Shape the draft into an admissible bloom — one approved member and the one
stage-catalog line the reducer requires (`PATCH` fields mirror `BloomDraft`):

```bash
curl -s -X PATCH localhost:8910/drafts/1 \
  -H 'content-type: application/json' \
  -d '{"proposals":[{"workpiece":"wp-1","scope_revision":[7,…],"approval":{"subject":[7,…],"kind":"Approval","detail":[9,…]}}],"base":[1,…],"stage_catalog":[…]}'
```

Seal it — the outcome names the sealed bloom id:

```bash
curl -s -X POST localhost:8910/drafts/1/seal   # → {"outcome":{"Sealed":[…32 bytes…]}}
```

Read the sealed bloom back — the whole view document, then one bloom by its
hex id:

```bash
curl -s localhost:8910/view                    # → {"mainline":[…],"blooms":[{…}]}
curl -s localhost:8910/blooms/<hex-of-the-32-bytes>
```

Read the journal (the seal is now a durable record) and fetch a referenced
artifact by its digest:

```bash
curl -s localhost:8910/journal                 # → {"records":[{"sequence":0,"idempotency_key":"…","event":{…}}]}
curl -s localhost:8910/artifacts/<digest>      # → the raw bytes, or 404
```

To supersede, seal the successor draft in the same call — the predecessor is
the path id:

```bash
curl -s -X POST localhost:8910/blooms/<predecessor-hex>/supersede \
  -H 'content-type: application/json' \
  -d '{"successor_draft":"2"}'                 # → {"outcome":{"Superseded":{…}}}
```

## How it works

The router claims a small set of path prefixes on the HTTP server cap and
dispatches every request through one handler that switches on method + path.
The in-memory shaping routes (workpieces, drafts) answer synchronously; the
durable routes forward a mail to a peer cap — the control core
(`aether.bloomery.admit` / `aether.bloomery.query`), the store
(`aether.store.replay_journal`), or the artifacts cap (`aether.artifacts.get`)
— and answer the HTTP client only when that reply lands, correlating the
deferred reply the same way the RPC and HTTP server caps do.
