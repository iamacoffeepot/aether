# Driving a bloom over the REST control API

The Bloomery coordinator ships a REST control ingress (ADR-0149 §Packaging):
a native `BloomeryApiCapability` router mounted on the `aether.http.server`
capability, so an operator authors a stored commission, shapes and seals a
draft, supersedes, and reads the live blooms / view document / journal /
artifacts from `curl`, with no typed-mail RPC vocabulary. The RPC ingress
stays mounted alongside it for fleet plumbing; this API is the human/shell
surface.

The coordinator now persists commissions in its first-party store and seals
from those rows. [ADR-0199](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0199-the-bloomery-owns-its-source-and-work-orders.md)
is still **Proposed**: that store and these routes exist in this binary, but
the fleet-wide source cutover is not accepted policy, and default boot still
selects the GitHub authority backend.

**Do not run this sealing tutorial against a live fleet coordinator.** A
warning is not isolation: `bloomery.db` at the repo root, default artifact
roots, and ambient `GITHUB_TOKEN` / `AETHER_GITHUB_*` / authority / local-lane
knobs can reuse live state. The launch below builds into a fresh trial
directory and starts that binary with `env -i` so only `PATH` and explicit
local-only knobs are visible. A seal admits work into that process's journal
only. This is a local REST seal demonstration, not a functioning source
pipeline.

## Booting the coordinator with the API

Commission authoring is fail-closed without a bearer: `AETHER_HTTP_CONTROL_TOKEN`
empty (the default) refuses every `/commissions` request with `401`. Other
host-local lifecycle routes stay unauthenticated on this bind.

The trial copies the shipped `approval-policy.toml` (it admits `docs/guide/**`
at `auto`) and points every store root at the trial directory. Local lanes
are off (`AETHER_GITHUB_LOCAL_LANE_ENABLED=false`) so nothing dispatches a
model. CAS landing is off (`AETHER_GITHUB_CAS_LAND_ENABLED=false`). GitHub
owner / repo / `GITHUB_TOKEN` are omitted, so the connection stays
unconfigured: remote reactors mount disabled, and `SourceCapability::seal_op`
is an offline no-op (`claims_enabled` is false; acquire/release never reach
the network). `AETHER_BLOOMERY_AUTHORITY_BACKEND=github` with those knobs
empty is not a live GitHub authority. Do not set
`AETHER_BLOOMERY_AUTHORITY_REPO`.

`AETHER_HTTP_PORT=0` and `AETHER_RPC_PORT=0` bind unused OS-assigned
localhost ports. Read the HTTP port this **owned** process logged; do not
assume `8910` and do not take `COORDINATOR` from ambient.

The subscriber (`tsfmt::Layer` on stderr) styles every field name even when
that stream is a file: italic SGR around `port` (`ESC[3m…ESC[0m`) and dim
SGR around `=` (`ESC[2m=ESC[0m`), so the four bytes `port=` never appear.
There is no production knob that turns that styling off. Strip SGR
(`ESC[` + digits + `m`) first, then the existing `port=` sed matches the
same announcement the harness already parses after CSI strip.

```bash
set -euo pipefail
REPO=$(git rev-parse --show-toplevel)
cd "$REPO"
TRIAL=$(mktemp -d "${TMPDIR:-/tmp}/bloomery-rest-seal.XXXXXX")
echo "trial directory: $TRIAL"
cp "$REPO/approval-policy.toml" "$TRIAL/approval-policy.toml"
mkdir -p "$TRIAL/worktrees" "$TRIAL/artifacts" "$TRIAL/archive"

CARGO_TARGET_DIR="$TRIAL/target" cargo build -p aether-chassis-bloomery --bin bloomery
BIN="$TRIAL/target/debug/bloomery"
test -x "$BIN"

TOKEN=local-only-example-token
: > "$TRIAL/bloomery.stderr"
env -i \
  PATH="$PATH" \
  AETHER_LOG_FILTER=info \
  AETHER_HTTP_PORT=0 \
  AETHER_RPC_PORT=0 \
  AETHER_STORE_PATH="$TRIAL/journal.sqlite" \
  AETHER_ARTIFACTS_ROOT="$TRIAL/artifacts" \
  AETHER_SESSION_DB_PATH="$TRIAL/sessions.sqlite" \
  AETHER_GITHUB_LOCAL_WORKTREE_BASE="$TRIAL/worktrees" \
  AETHER_BLOOMERY_ARCHIVE_BASE="$TRIAL/archive" \
  AETHER_APPROVAL_POLICY_FILE="$TRIAL/approval-policy.toml" \
  AETHER_HTTP_CONTROL_TOKEN="$TOKEN" \
  AETHER_GITHUB_LOCAL_LANE_ENABLED=false \
  AETHER_GITHUB_CAS_LAND_ENABLED=false \
  AETHER_BLOOMERY_AUTHORITY_BACKEND=github \
  "$BIN" >>"$TRIAL/bloomery.stderr" 2>&1 &
pid=$!

stop_owned() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

http_port=
for _ in $(seq 1 50); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "coordinator exited before it bound HTTP; last log:" >&2
    tail -n 24 "$TRIAL/bloomery.stderr" >&2
    stop_owned
    exit 1
  fi
  http_port=$(
    sed $'s/\x1b\\[[0-9;]*m//g' "$TRIAL/bloomery.stderr" \
      | sed -n 's/.*http server bound.*port=\([0-9][0-9]*\).*/\1/p' \
      | tail -n 1
  )
  if [ -n "$http_port" ]; then
    break
  fi
  sleep 0.2
done
if [ -z "$http_port" ]; then
  echo "coordinator never announced an HTTP port (bind collision or boot hang); last log:" >&2
  tail -n 24 "$TRIAL/bloomery.stderr" >&2
  stop_owned
  exit 1
fi

COORDINATOR="http://127.0.0.1:$http_port"
ready=0
for _ in $(seq 1 50); do
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "coordinator exited before /drafts and /view answered; last log:" >&2
    tail -n 24 "$TRIAL/bloomery.stderr" >&2
    stop_owned
    exit 1
  fi
  if curl -fsS --connect-timeout 1 --max-time 2 "$COORDINATOR/drafts" >/dev/null \
    && curl -fsS --connect-timeout 1 --max-time 2 "$COORDINATOR/view" >/dev/null; then
    ready=1
    break
  fi
  sleep 0.2
done
if [ "$ready" != 1 ]; then
  echo "boot failed or address collision on $COORDINATOR; last log:" >&2
  tail -n 24 "$TRIAL/bloomery.stderr" >&2
  stop_owned
  exit 1
fi

printf 'COORDINATOR=%s\nTOKEN=%s\n' "$COORDINATOR" "$TOKEN" > "$TRIAL/curl.env"
echo "owned pid $pid on $COORDINATOR"
```

The startup line `bloomery REST control api mounted policy_loaded=true` in
`$TRIAL/bloomery.stderr` confirms the auto-approval door and a draft that
seals no policy of its own have a fallback. Without it both refuse
`approval policy unavailable; … fails closed`.

`GET /view` always carries a `mainline` digest. On this fresh trial journal
that is the genesis all-zero sentinel the control core starts from. Capture
the returned value. This process has no usable git tree.

A second terminal must source the **same** trial file — not ambient
`COORDINATOR` / `AETHER_HTTP_CONTROL_TOKEN`, which may point at a fleet:

```bash
set -euo pipefail
# TRIAL is the directory the boot terminal printed.
. "$TRIAL/curl.env"
: "${COORDINATOR:?}" "${TOKEN:?}"
```

Do not delete the trial directory or its `target/` from the recipe. After
you are done, stop the process from the **same boot shell** that still holds
`$pid` (`kill "$pid"; wait "$pid" || true`). Do not kill a number reread from
a pid file — that slot may already belong to someone else. Then remove
`$TRIAL` yourself if you no longer need the journal, artifacts, or build
tree.

## The route table

| Method & path | Effect |
|---|---|
| `POST /commissions` | Persist a new open commission. **Bearer required.** `201` `{id,intent}`. |
| `GET /commissions` · `GET /commissions/{id}` | List heads (`?status=` optional) / show one commission. **Bearer required.** |
| `POST /commissions/{id}/revisions` | Store a scope revision. **Bearer required.** `201` `{digest}`. |
| `POST /commissions/{id}/approvals` | Verify a signed Approve-door statement and store it. **Bearer required.** |
| `POST /commissions/{id}/approvals/auto` | Mint the unsigned auto-tier approval from the stored revision. Empty body. **Bearer required.** |
| `POST /commissions/{id}/cancel` · `POST /commissions/{id}/reopen` · `POST /commissions/{id}/scope-runs` | Close, restore, or open a scoping run. **Bearer required.** |
| `POST /workpieces` | Stage an in-memory workpiece handle. This is **not** seal authority. |
| `GET /workpieces` | List durable open commissions that already have a current revision. |
| `POST /drafts` | Open an empty draft; returns its handle (`draft_id`). |
| `GET /drafts` · `GET /drafts/{id}` | List / read open drafts. |
| `PATCH /drafts/{id}` | Replace the present fields of a draft (membership, base, configuration registry, forecast). |
| `POST /configs` | Canonically encode and durably store a configuration by kind; returns its content address. |
| `GET /configs/{digest}` | Read a stored configuration back as JSON. |
| `POST /drafts/{id}/seal` | Load each member from the commission store, run the approve gate, freeze the draft to a `BloomSpec`, and admit `Fact::Seal`. The body is optional; caller projections and descriptions are not authority. |
| `POST /blooms/{id}/supersede` | Seal the named successor draft and admit `Fact::Supersede` against the `{id}` predecessor. |
| `POST /blooms/{id}/grant` | Hand a wedged member more attempts and resume it on the `{id}` bloom, without sealing anything. |
| `POST /blooms/{id}/answer/{question}` | Adopt an owner-signed answer to the parked question `{question}` names, releasing the hold it took. |
| `POST /blooms/{id}/members/{workpiece}/withdraw` | Take one member out of the walking `{id}` bloom: cancel its lane, free its claim ref, and stop the folds waiting on it. |
| `GET /blooms` · `GET /view` | The whole live view document. |
| `GET /blooms/{id}` | One bloom's live view (`{id}` is the bloom's hex digest). |
| `GET /blooms/{id}/why` | Why the `{id}` bloom is not advancing: a chain from the land down to member dispatch, each rung naming the one below it, plus one answer per member. |
| `GET /claims` | Every live claim ref and the bloom holding it. |
| `POST /claims/releases` | Authorize releasing one orphaned claim ref with an author signature; returns `202` and the request digest. |
| `GET /claims/releases/{digest}` | One authorized release's state — pending, or its terminal result. |
| `GET /journal` | The whole journal, decoded, oldest first. |
| `GET /artifacts/{digest}` | The content-addressed artifact bytes, or `404`. |
| `POST /archive` | Move eligible evidence directories and resolved session trees onto the archive tier (ADR-0211). Refuses with `409` unless the coordinator is between blooms. Nothing is ever deleted. |
| `GET /archive` | List the records currently on the archive tier. |

Request and response bodies are JSON over the `aether-bloomery` value types
(`Workpiece`, `BloomDraft`, `Membership`, `ViewDocument`, `ScopeRevision`,
`Statement`, …) via serde. Three representation notes carry from those types:

- **Digests** (a stored intent, a scope revision, a bloom's `base`, a
  `BloomId`) are 64 lowercase **hex** characters wherever they appear — a
  path segment, a request body, a response body. The seal outcome hands the
  sealed id back in exactly the spelling `/blooms/{id}` takes.
- **A request body also accepts the canonical form**: the 32-element byte
  array serde renders `Digest([u8; 32])` as. Either spelling resolves to the
  same bytes before anything downstream sees it. Hex that is the wrong length
  or carries a non-hex character is a `400` naming the field, never a partial
  read. `Statement.words` is a byte array, not a digest spelling, even when
  it is 32 bytes long.
- **Configuration registries** map a kind name to the content address
  returned by `POST /configs`. A draft with no stage-catalog entry uses the
  compiled default line. The same registry carries the approval policy under
  `aether.bloomery.approval_policy`, and that one is **bloom-wide only**
  ([ADR-0174](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0174-bloom-configuration-is-a-kind-keyed-registry.md)):
  a member sealing its own entry would pick the tier deciding whether that
  member may be admitted, so the seal is refused outright. A present entry
  whose kind or content the host cannot resolve fails loudly rather than
  silently falling back.

Stored commissions are the seal's authority. `POST /workpieces` still stages
an in-memory handle, but a draft that names a workpiece with no open stored
commission is refused (`member {id} has no commission in the store; seal
fails closed`). The draft's membership still names the workpiece id and the
exact stored revision digest; the gate reconstructs surface, completeness,
description, and approval from the store.

## A curl walkthrough

This is a minimal Auto-tier `docs/guide/**` seal on the trial coordinator
above. `curl -fsS` fails the script on HTTP errors. `--connect-timeout` and
`--max-time` bound a hung TCP connect so retries cannot wait forever.
Capture returned ids with `jq -er` so a missing or null field is a failure.
Do not invent placeholder digests, assume the first draft is `"1"`, or
default `COORDINATOR` from ambient.

```bash
set -euo pipefail
: "${COORDINATOR:?source $TRIAL/curl.env from the boot terminal}"
: "${TOKEN:?same local-only token the trial coordinator booted with}"
WP=wp-guide

base=$(curl -fsS --connect-timeout 2 --max-time 10 "$COORDINATOR/view" | jq -er '.mainline')
```

`base` is this trial coordinator's `GET /view` mainline. On the fresh trial
journal that is the genesis sentinel. Capture the returned value; do not
invent a git sha.

Create the commission. The intent is a `Statement`: `words` are UTF-8 bytes,
`provenance` is an observation (create does not verify a signature), and
`parents` is empty. The `201` body names the stored intent digest:

```bash
created=$(
  jq -n --arg id "$WP" --arg text "Correct the REST seal walkthrough." '{
    id: $id,
    intent: {
      words: ($text | explode),
      provenance: {ObservationAttestation: {source: "rest-walkthrough-local"}},
      parents: []
    }
  }' | curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/commissions" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    --data-binary @-
)
echo "$created" | jq .
intent=$(echo "$created" | jq -er '.intent')
```

Write a complete version-1 `ScopeRevision`. `schema` is `1`. `routing` is the
JSON object `{size, model}`, not a markdown blob. `implements`,
`declared_crates`, and `declared_reads` are required fields; empty arrays are
the glob-declared Auto case. `description` must be non-empty — the seal
refuses an empty one. Completeness is **not** a request field: at seal the
store reconstructs it from these bytes (non-empty problem / design / plan,
exactly one model routing, open status, fresh tip, closed dependencies).

```bash
written=$(
  jq -n --arg id "$WP" '{
    revision: {
      schema: 1,
      workpiece: $id,
      predecessor: null,
      problem: "The REST seal walkthrough staged an in-memory workpiece and could not seal.",
      design: "Store a commission, write a complete scope revision, auto-approve, then seal.",
      plan: "Author the commission over REST, then seal one auto-tier docs/guide member.",
      declared_surface: ["docs/guide/**"],
      dogfood_brief: "N/A",
      routing: {size: "S", model: "construct: test"},
      dependencies: [],
      description: "Correct the REST seal walkthrough.",
      implements: [],
      declared_crates: [],
      declared_reads: []
    }
  }' | curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/commissions/$WP/revisions" \
    -H "Authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    --data-binary @-
)
echo "$written" | jq .
revision=$(echo "$written" | jq -er '.digest')
```

The `201` `{digest}` is the stored revision's content address — the value the
draft must pin and the auto-approval binds. Auto-approval is a separate
authenticated door, and it has to run **before** seal. The caller sends no
statement and no signature: the door loads the stored revision, resolves
`declared_surface` / `declared_crates` against the host file policy, and mints
an `ObservationAttestation` whose words are that revision digest. A surface
that resolves above `auto` is `422` naming the tier it found.

```bash
approved=$(
  curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/commissions/$WP/approvals/auto" \
    -H "Authorization: Bearer $TOKEN"
)
echo "$approved" | jq .
approval=$(echo "$approved" | jq -er '.digest')
```

Open a draft and read the handle it mints:

```bash
opened=$(curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/drafts")
echo "$opened" | jq .
draft_id=$(echo "$opened" | jq -er '.draft_id')
```

Shape the draft from the returned ids. The membership's `approval` is a
reducer-shaped placeholder the gate overwrites; it is not authority. Omit
bloom-wide `configs` to use the compiled default stage line — there is no
`catalog.json` on this path.

```bash
jq -n --arg wp "$WP" --arg revision "$revision" --arg base "$base" '{
  proposals: [
    {
      workpiece: $wp,
      scope_revision: $revision,
      configs: {entries: {}},
      approval: {
        subject: "0000000000000000000000000000000000000000000000000000000000000000",
        kind: "Approval",
        detail: "0000000000000000000000000000000000000000000000000000000000000000"
      }
    }
  ],
  base: $base
}' | curl -fsS --connect-timeout 2 --max-time 10 -X PATCH "$COORDINATOR/drafts/$draft_id" \
  -H 'content-type: application/json' \
  --data-binary @- | jq .
```

Seal it. An empty object is enough: stored commission rows supply surface,
completeness, description, and approval. A body that still carries
`projections` or `descriptions` is accepted and those fields are ignored.

```bash
sealed=$(
  curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/drafts/$draft_id/seal" \
    -H 'content-type: application/json' \
    -d '{}'
)
echo "$sealed" | jq .
bloom_id=$(echo "$sealed" | jq -er '.outcome.Sealed')
# A 200 SealRejected body is not a sealed id; jq -er fails on null.
```

The outcome names the sealed bloom as hex:

```json
{"outcome":{"Sealed":"<64-hex-from-jq>"}}
```

A projection is no longer a caller input. The store-backed reader matches the
draft membership to the current open commission by workpiece id and exact
scope digest, then reconstructs the gate's facts:

- `declared_surface` / `declared_crates` / `declared_reads` — copied from the
  frozen revision. Empty `declared_crates` means the surface is glob-declared,
  so `docs/guide/**` takes the file's `auto` rule. A non-empty crate list
  resolves tier from protected files instead.
- `completeness` — the nine facts the gate fails closed on, reconstructed:
  non-empty problem / design / plan, `referenced_adr_prs_merged` held,
  `model_routing_count` exactly `1`, `blocked` false, tip fresh,
  dependencies co-sealed or landed, umbrella integrity held.
- `adr_touch` — `"None"`, `"ProposedOnly"`, or `"NewOrEstablished"` from the
  ADRs at the sealed `base`. `NewOrEstablished` routes to the owner ahead of
  any policy lookup.
- `description` — the revision's non-empty work-order text, which construct
  uses as `## Task`. It is not a seal-body field.

The gate fails closed at every branch, so a `422` names what fell short —
`member wp-guide has no stored approval; seal fails closed` if auto-approval
was skipped, and a draft with no members is an empty seal rather than a
missing-projection error.

Read the sealed bloom back — the whole view document, then one bloom by the
hex id the seal returned:

```bash
curl -fsS --connect-timeout 2 --max-time 10 "$COORDINATOR/view" | jq .
curl -fsS --connect-timeout 2 --max-time 10 "$COORDINATOR/blooms/$bloom_id" | jq .
```

### Optional: authoring configuration

The compiled default stage line is enough for the walkthrough above. To attest
a stage catalog or the tier policy the bloom was admitted under, author the
value through `POST /configs` and name it bloom-wide on the draft
([ADR-0174](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0174-bloom-configuration-is-a-kind-keyed-registry.md)).
That is a separate recipe from the minimal Auto path, and it is not required
for `docs/guide/**` when the host file already admits that glob.

The envelope is `{"kind":"<Kind::NAME>","value":{…}}`. `value` must be the
full JSON document for that kind — a missing catalog file or an empty object
is not a catalog. The `200` `{digest,kind}` is the address a draft registry
seals under. A partial `PATCH` preserves the existing registry, while a
present `configs` object replaces it.

To have the bloom carry the policy it was admitted under rather than inherit
the coordinator's file, author one and name it bloom-wide **before** seal.
`POST /commissions/{id}/approvals/auto` still resolves the **host file**,
because a lone commission has no bloom-wide registry. `default` and each
rule's `tier` are `"Auto"`, `"Judge"`, or `"Human"` — the JSON spelling of
the tier enum, not the lowercase words the fallback file uses. Rules are
unordered; resolution is most-restrictive-wins over the declared surface.

```bash
policy=$(
  curl -fsS --connect-timeout 2 --max-time 10 -X POST "$COORDINATOR/configs" \
    -H 'content-type: application/json' \
    -d '{"kind":"aether.bloomery.approval_policy","value":{"default":"Judge","rules":[{"glob":"docs/guide/**","tier":"Auto"}]}}' \
    | jq -er '.digest'
)

jq -n --arg digest "$policy" \
  '{"configs":{"entries":{"aether.bloomery.approval_policy":$digest}}}' \
  | curl -fsS --connect-timeout 2 --max-time 10 -X PATCH "$COORDINATOR/drafts/$draft_id" \
    -H 'content-type: application/json' \
    --data-binary @-
```

Five ways a seal is refused on this axis, all `422` and all before any admit:

| Response | Cause |
|---|---|
| `member wp-guide seals its own approval policy; the tier policy is bloom-wide only, seal fails closed` | The entry sits in a member's registry rather than the draft's. |
| `sealed approval policy unresolvable: …` | The bloom-wide address names content that is missing, filed under another kind, or no longer decodes. |
| `configuration set not yet read; seal fails closed` | The seal arrived before the coordinator finished its boot configuration read. Retry. |
| `approval policy unavailable; seal fails closed` | The draft seals no policy and the fallback file did not load. |
| `member wp-guide declared surface "crates/aether-fs/src/lib.rs" names one file and no approval-policy rule names that file; widen it to a crate glob such as crates/<crate>/src/**; seal fails closed` | The declared surface names an individual file the policy does not. |

### Signed doors (ADR-0182)

`POST /commissions/{id}/approvals/auto` is unsigned on purpose: an observation
that the host file resolved `auto`, never an author signature. Above-`auto`
work uses `POST /commissions/{id}/approvals` with an owner-signed `Statement`
instead.

What the owner signs is not the statement's `words` on their own. Every signed
door verifies against the digest of an *authorization*: which door the
signature is for, the exact request digest it is good for, and the words
together
([ADR-0182](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0182-signed-authorizations-bind-their-request.md)).
An approve statement is signed for the Approve door bound to the member's
`scope_revision`, and an answer is signed for the Answer door bound to the
question digest the route names. A signature therefore authorizes one request
at one door — re-pointing an envelope at a different question, revision, or
ref produces no verifying signature, where a statement's `parents` alone never
could, being outside the signature and rewritable by whoever holds it.
Statements signed against the older words-only message do not verify and must
be re-signed.

A verifying signature is not the whole gate at the answer door. The admitted
fact carries no question field of its own, so the coordinator picks the hold
to release out of the answer statement's `parents` — which is outside the
signature. `POST /blooms/{id}/answer/{question}` therefore also requires
`parents` to be exactly one digest, the same question the path names, and
answers `400` for anything else. A list naming a different question is
refused, and so is one that merely *includes* the path question: the release
scan takes the first parent that is an open hold in the order you wrote them,
so `[other, question]` would release `other`. Set `parents` to `[question]`
and nothing else.

Read the journal (the seal is now a durable record) and fetch a referenced
artifact by its digest:

```bash
curl -fsS --connect-timeout 2 --max-time 10 "$COORDINATOR/journal" | jq .   # → {"records":[{"sequence":1,"idempotency_key":"…","event":{…}}]}
curl -fsS --connect-timeout 2 --max-time 10 "$COORDINATOR/artifacts/<digest>"   # → the raw bytes, or 404
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
  -d '{"workpiece":"4708","stage":"Verify","attempts":2,"reason":"sandbox recovered","operator":"eve"}'
                                               # → {"outcome":{"AttemptsGranted":{…}}}
```

`attempts` is how many more dispatched attempts the member may spend before it
wedges again, bounded by the stage's own retry budget in the sealed stage
catalog — the whole retry authority, with no second bloom-wide cap layered over
it. `reason` and `operator` are required, as on the other operator doors: a
grant is an act no verdict produced, so a blank audit trail is `422` rather
than defaulted. A reducer refusal of the grant (not wedged, wrong stage, past
the cap) is `422` too. A `Verify` grant resumes the member at `Refine`, since
re-running the mechanical gate on an unchanged candidate cannot change its
verdict.

The sealed `base` is what divides the two verbs. A base that has not moved, with
scope, membership, and configuration unchanged, is an execution decision — a
grant. A moved base, or changed scope, membership, or configuration, is a
successor doing real work — a supersession.

## When the whole-bloom review could not run

A member wedge answers for a member. The bloom-scope counterpart is the
whole-bloom review reporting that its executor could not judge the fold at all —
a sandbox that would not start, a checkout the host could not materialize. No
candidate was read, so this is not a review finding and it opens no member
repair lap; it lands on the bloom's own view instead:

```json
"executor_fault": {
  "subject": [ … ],   // the fold tree the faults are against
  "rolls": 2,         // faults taken on that fold
  "budget": 2,        // the sealed AggregateReview retry budget
  "evidence": [ … ],  // the latest fault report's artifact digest
  "terminal": true    // the series reached its ceiling
}
```

The field is absent on an ordinary bloom. Below the budget the coordinator
re-dispatches the same review against the same held fold under a fresh order, so
a transient outage costs nothing but time. At the budget (`terminal: true`) it
stops: no further dispatch, the fold and every member claim still held, and the
members exactly where they were.

That stop is deliberate and has no in-band release. Repairing the host is
operational authority rather than a decision about the work, so there is no
question to answer and no grant to make — a `terminal: true` bloom recovers by
repairing the environment and then sealing a successor:

```bash
curl -s -X POST localhost:8910/blooms/<bloom-hex>/supersede \
  -H 'content-type: application/json' \
  -d '{"successor_draft":"2"}'
```

Fetch the fault report itself the way you fetch any artifact —
`curl -s localhost:8910/artifacts/<hex-of-evidence>` — to read what the lane
said it could not do.

## The archive tier

ADR-0211 classifies every artefact the coordinator produces as a record, working
state, or a cache. **No coordinator path deletes a record.** Evidence directories
and session trees stay until an operator archives them; archival changes where
a record lives, never whether it exists.

The three classes, and where each goes:

| Class | What | Where it lives | Who moves it |
|---|---|---|---|
| Record | Evidence directories, session trees, the journal, the artifact store | Working root, then the archive tier | `POST /archive` / `cargo xtask bloom archive` |
| Working state | Nonce-keyed dispatch checkouts, terminal working refs | Reclaimed between blooms by the janitor tick | The tick, never the archive pass |
| Cache | Per-slot cargo target directories | The configured `lane_target_base` | Disk-pressure eviction |

The pass refuses unless the coordinator is between blooms: no bloom active and
unlanded, and no order outstanding. A `409` names the walking bloom or the
outstanding nonce. While work walks, a session tree renamed out from under a
resumable conversation is the 2026-08-25 board-5435 failure; the operator stays
the trigger for that reason.

Layout under `AETHER_BLOOMERY_ARCHIVE_BASE` (empty resolves to
`<local_worktree_base>/archive`):

```
<archive-base>/
  evidence/<nonce>-evidence/
  sessions/<slug>/
```

Each record keeps the name it was addressed by. `GET /archive` is a directory
listing of that tree. An archived dispatch still reads through
`GET /dispatches/{nonce}` and its transcript page: the header reports
`retained: true`, names the tier path in `archived`, and carries no swept
notice.

`cargo xtask bloom archive` posts the pass; `--list` enumerates the tier.
A refusal exits non-zero so a scripted run does not read a `409` as success.

## How it works

The router claims a small set of path prefixes on the HTTP server cap and
dispatches every request through one handler that switches on method + path.
Draft shaping is in-memory and answers synchronously. `POST /workpieces` still
stages an in-memory handle; `GET /workpieces` lists durable open commissions,
and `POST /drafts/{id}/seal` loads each member from the commission store
before it admits. Other durable routes forward a mail to a peer cap — the
control core (`aether.bloomery.admit` / `aether.bloomery.query`), the store
(`aether.store.replay_journal`), or the artifacts cap (`aether.artifacts.get`)
— and answer the HTTP client only when that reply lands, correlating the
deferred reply the same way the RPC and HTTP server caps do.

## Releasing an orphaned claim ref

A claim ref outlives the journal that created it — that is what makes it work
across instances. So any journal lifetime shorter than the claim's leaves a ref
whose holder no surviving snapshot knows: a trial coordinator whose store was
discarded, a journal reset, a restore to a snapshot older than the seal. Boot
reconcile deliberately leaves such a holder alone (absence from *this* journal is
not proof another instance's bloom is dead), and supersession needs the
predecessor locally, so an orphaned mainline-admission ref refuses every later
seal against that mainline:

```json
{"outcome": {"SealRejected": {"ActiveBloomExists": "…ea019763…"}}}
```

Start by looking at the refs. `GET /claims` is the read surface that used to
require leaving the API for `git ls-remote`:

```bash
curl -s localhost:8080/claims | jq
```

```json
{"claims": [
  {"ref_kind": "MainlineAdmission", "holder": {"Held": "/* the holder bloom id, in hex */"}},
  {"ref_kind": {"Workpiece": "wp-trial-hop"}, "holder": {"Held": "/* … */"}}
]}
```

**Enumeration is diagnostic, not a liveness oracle.** A holder this instance does
not know may be another instance's live bloom, mid-run. Investigate the holder
before you go further — the machine cannot tell the two apart, which is exactly
why the next step needs your signature rather than a flag.

Once you have satisfied yourself the holder is dead, sign the release. The
request names one typed ref and one expected holder; there is no ref-path field,
so no spelling of this body reaches a Git ref outside the claim namespace. The
authorizing statement's words must be exactly `release orphan bloomery claim`,
and you sign it for the orphan-claim-release door bound to the request's own
content digest — the digest of the `{ref_kind, expected_holder}` pair the body
names, which the coordinator recomputes from that body rather than reading out of
your envelope. That signed binding is what keeps one signature from authorizing a
second, different release. `parents` must still name that same request digest —
the coordinator refuses the request otherwise — but it now records the derivation
edge alongside a binding the signature already covers, rather than carrying the
authorization on its own.

```bash
curl -s -X POST localhost:8080/claims/releases -d @- <<'JSON' | jq
{
  "ref_kind": "MainlineAdmission",
  "expected_holder": "/* the hex holder from GET /claims */",
  "authorization": {
    "words": [/* utf-8 bytes of: release orphan bloomery claim */],
    "provenance": {"AuthorSignature": {"signer": "operator", "signature": [/* … */]}},
    "parents": ["/* the request digest, in hex */"]
  }
}
JSON
```

A `202` carries the request digest. Poll it:

```bash
curl -s localhost:8080/claims/releases/<digest> | jq
```

```json
{"target": {"ref_kind": "MainlineAdmission", "expected_holder": [/* … */]},
 "completion": "Released"}
```

`completion` is `null` while the release is still pending, then one of three
terminals:

- **`Released`** — the expected holder's ref was deleted. Seals against that
  mainline work again.
- **`AlreadyAbsent`** — the ref was already gone. A success, not a failure: it is
  also what a redrive reports after a crash between the deletion and its
  journaled completion, which is what makes the same authorized request safe to
  finish rather than permanently stuck.
- **`Changed`** — the ref exists under a *different* holder, so nothing was
  touched. The expected-holder compare-and-swap protected a ref that moved under
  you. Re-read `GET /claims` and decide again; the release never retries against
  a holder you did not name.

Four refusals are synchronous, and none of them attempts a mutation: a body that
does not decode (`400`), a signature that does not verify against the host's
signer allowlist (`400`), an authorization whose words or parents do not bind
this request, and an `expected_holder` that *is* a bloom this journal knows. The
last one is the important boundary — a known holder belongs to the ordinary
lifecycle (reconcile, supersede, the land-time release), and this route must
never become a second, unaudited way to free the claims of a bloom that is still
working.

Every release is journaled: the signed request, its digest, and its terminal
result. Resubmitting the same request returns the same digest and enqueues
nothing, so a retried call cannot release twice.


## Withdrawing one member

`supersede --eject` is a draft edit, not a member-removal mechanism. It seals a
whole successor: a new bloom id, a re-adoption of every resolved sibling, a
fresh claim transfer, and a new work order — all to shed one member whose scope
turned out wrong. `POST /blooms/{id}/members/{workpiece}/withdraw` is the narrow
move instead. The bloom keeps its id, its sealed base, and every sibling's
finished work; only the named member leaves.

```bash
curl -sS -X POST "$COORDINATOR/blooms/$BLOOM/members/issue-4291/withdraw" \
  -H 'content-type: application/json' \
  -d '{"reason": "the scope was wrong; re-scoping it for tomorrow", "operator": "ops"}'
```

Like every other bloom operator route this one is unauthenticated on the
host-local bind, and like every other one it refuses a body that says nothing:
`reason` and `operator` are both required and both non-blank, because an act no
verdict produced has its audit trail as its whole product. A blank one is `422`.

What a withdrawal does, and what it deliberately leaves alone:

- **The folds stop waiting.** Three of them are otherwise total over the sealed
  member list — the claim-completeness scan, the candidate list the fold is
  built from, and the resolve gate — which is exactly why one member that will
  never produce a claim pins the bloom, its siblings' finished work, and the
  mainline behind it. If the withdrawal completes the remaining claim set, the
  fold dispatches on the spot.
- **The lane is killed, and nothing is charged for it.** The member's
  outstanding orders are cancelled and consumed. No evidence is admitted and no
  attempt is spent — a synthesised timeout verdict would route a member that has
  left the line into a repair lap.
- **One ref is freed, not the whole seal.** `refs/bloomery/claims/<workpiece>`
  is released on its own; `refs/bloomery/admission/mainline` stays with the
  bloom, which is still walking. Freeing the workpiece is the point: it is what
  lets the member be re-scoped and sealed into a later bloom.
- **Nothing else moves.** No sibling's claim is revoked, no budget is handed
  back, no cursor of anyone else's moves, and the bloom does not un-resolve.

A member that already carries a resolution claim cannot be withdrawn: reviewed
work is immutable (ADR-0191 §4), and pulling it out from under a fold that has
already counted it is the failure that rule exists to prevent. That is `422`,
naming the member.

### Dependents, and why the refusal is fail-closed

A member that declares a construct-base dependency on a withdrawn one can never
enter the line — its base will never exist. Parking it would not help: a parked
dependent blocks the resolve gate exactly as an unclaimed member does, so the
bloom stays pinned by the very member the withdrawal was meant to free. So the
door refuses instead, naming the dependents, unless the request opts in:

```bash
curl -sS -X POST "$COORDINATOR/blooms/$BLOOM/members/issue-4291/withdraw" \
  -H 'content-type: application/json' \
  -d '{"reason": "the scope was wrong", "operator": "ops", "cascade": true}'
```

A cascaded dependent is withdrawn with a derived cause naming the ancestor that
stranded it, which is the visible reason an operator reads on the board rather
than a member sitting silently on a base that will never arrive.

### It is one-way, and it needs a coordinator that knows the verb

There is no un-withdraw, deliberately. A member wrongly withdrawn is re-scoped
and sealed into a later bloom — which is what releasing its claim ref makes
possible in the first place.

The verb changes the reducer and the executor reactor, both of which are
compiled into the running coordinator, so a live coordinator holding an older
binary has nowhere to admit the fact. Replace it first with
`cargo xtask bloom upgrade --candidate … --bin … --store …`, which fold-tests
the candidate over a copy of the live journal before installing and restarting
the unit. Every wire addition here is appended, so that replay reads existing
rows unchanged.

The CLI verb is the same act through the same door:

```bash
cargo xtask bloom withdraw "$BLOOM" issue-4291 \
  --reason 'the scope was wrong; re-scoping it for tomorrow' --operator ops
```
