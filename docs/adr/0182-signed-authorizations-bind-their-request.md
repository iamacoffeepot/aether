# ADR-0182: Signed authorizations bind their request

- **Status:** Accepted
- **Date:** 2026-08-12

## Context

ADR-0149 §The value vocabulary makes an author signature the only provenance that becomes instruction, and gives a `Statement` three fields: `words`, `provenance`, and `parents`. `Statement::verify_authority` (`crates/aether-bloomery/src/values/statement.rs`) hands `&self.words` to the `KeyProvider` as the signed message. `parents` sits outside the signed bytes.

Every door that admits a signed statement therefore binds it to a request *structurally*, by reading `parents` — a field any holder of the envelope can rewrite without disturbing the signature. Three doors exist or are in flight, and they do not share a fate:

- **Approve** (`precheck_statement` / `approval_from_statement`, `crates/aether-chassis-bloomery/src/bloomery/approve/statement.rs`) requires `statement.words == subject.as_bytes()` — the member's scope-revision digest. The binding sits *inside* the signed bytes, so a statement signed for one revision cannot approve another. This door is already sound, and it is the existing proof that the shape works.
- **Answer** (`reduce_adopt_answer`, `crates/aether-bloomery/src/reduce/evidence.rs`) signs the answer text and binds by `answer.parents` naming an open hold. It is sound only while every answer's text is unique across every open hold. Nothing enforces that: two parked questions answered `yes` produce identical signed bytes, so the first envelope re-parents onto the second hold and adopts it without the operator ever seeing the second question.
- **Orphan-claim release** (ADR-0179, in flight as pull request #4814) signs the fixed constant `release orphan bloomery claim` and binds by `parents.contains(&self.request())`. Its signed bytes never vary, so one author-signed statement verifies forever and can be re-pointed at any `(ref_kind, expected_holder)` pair. The independent review on that pull request called this the most general instance of the replay class in the codebase: re-parenting a captured release statement defeats the reducer's parent binding entirely, and a single legitimate release authorization becomes a universal release token for that door.

A fourth consumer of signed statements, the prompt-manifest closure walk (`ground_instruction`, `crates/aether-bloomery/src/manifest.rs`), is not a request door — it grounds an instruction slot rather than authorizing an act, and it already refuses to trust caller-declared parents, walking only `ProvenanceIndex::parents`. Its binding is not at risk, but it does verify signatures, and it holds no request of its own to bind one against; the Decision below therefore has to say where it gets one.

Two facts make the exposure durable rather than momentary. `Fact::AdoptAnswer` carries its `Statement` into the journal, `GET /journal` decodes and serves the whole journal, and ADR-0179 adds `Fact::RequestOrphanClaimRelease` carrying its authorization the same way — so a spent envelope is readable back out by anyone who can read the journal. And the reducer's live-holder refusal still stands in front of the release door, which narrows today's exposure to orphaned refs, the exact surface that door exists to operate on.

One property of the storage layer constrains every available answer. Journal records encode with `aether_data::wire` (ADR-0118), whose struct decode reads exactly `fields.len()` values positionally, with no field presence on the wire and no `deserialize_ignored_any` path. Adding a field to `Statement` makes every persisted record that carries one undecodable, which strands a coordinator holding a parked question at replay. The `Fact` enum already carries this discipline explicitly — new variants are appended so prior discriminants are unchanged — and it applies with equal force to the value types those variants carry.

## Decision

The message an author signature covers stops being the words alone and becomes the digest of a typed authorization subject.

```rust
pub enum AuthorityDoor { Approve, Answer, OrphanClaimRelease, Ground }

struct Authorization<'a> {
    door: AuthorityDoor,
    binding: Digest,
    words: &'a [u8],
}

impl ContentAddressed for Authorization<'_> {
    const DOMAIN: &'static str = "aether.bloomery.authorization";
}
```

`Statement::verify_authority` takes the door and the binding as required parameters and verifies the envelope over `digest_of(&Authorization { door, binding, words })`. Three properties follow:

1. **The binding is signed.** A signature authorizes one request digest and no other. Rewriting `parents` changes the artifact's address without producing a signature that verifies against the new target.
2. **Doors do not share signatures.** `AuthorityDoor` is a closed enum hashed into the subject, so an envelope minted for one door never verifies at another even where words and binding coincide. Adding a door is a variant, and the variant is what keeps its envelopes separate.
3. **No door can be built unbound.** The binding is a parameter with no default and there is no remaining verification path over words alone, so a future door cannot repeat this mistake by omission.

`Statement` keeps its three fields and its wire shape. `parents` keeps the derivation-DAG meaning ADR-0149 gives it — every artifact names its parents — and every existing structural check over it stays exactly as written. The signature binding is added in front of those checks, never in place of them: the reducer holds no key material and its parent check remains the only binding it can evaluate on replay, so removing it would trade a cryptographic gain for a structural loss.

The `aether.signing.verify` mail (`crates/aether-chassis-bloomery/src/signing/kinds.rs`) gains the door and the binding, and each host route supplies a binding it derived independently of the envelope:

- **Approve** binds to the member's `scope_revision`, which the seal path already holds at both the synchronous and deferred verification sites. The existing `words == subject.as_bytes()` precheck stays.
- **Answer** binds to the adopted question's digest. `POST /blooms/{id}/answer` must therefore name the question it answers, because today the reducer discovers the question by scanning `parents` after verification has already happened. The route learns the question digest, binds the signature to it, and the reducer's hold scan is then a re-check of something the signature already fixed.
- **Orphan-claim release** binds to `OrphanClaimRelease::request()`, recomputed by the host from the typed target in the request body rather than read from the envelope.

The prompt-manifest closure walk gets its binding a different way, because it has no request to derive one from. `ProvenanceIndex` grows `authority_binding(&Digest) -> Option<(AuthorityDoor, Digest)>`: the host records what a statement was verified against when it admitted the statement, and the walk recovers that record and re-runs the same cryptographic check with it under the `Ground` door. A statement the index cannot answer for leaves the node ungrounded, so the walk stays fail-closed and its check keeps its cryptographic character rather than degrading to a structural one. Inventing a binding for it would have been worse than useless — the walk would verify a signature against a request the signer never agreed to.

## Migration

Existing signed envelopes stop verifying and must be re-signed. There is no dual-accept window: a window that still accepts the legacy words-only message accepts exactly what a captured envelope offers, so it would hold the vulnerability open for its whole duration while advertising that it was closed.

The cost of the hard cutover is small and bounded because `Statement`'s wire shape does not move. Journals replay unchanged, no `Fact` discriminant shifts, and no stored digest is recomputed. No tool in this repository mints envelopes, so no signer is silently left on the old message. Approval and answer statements are supplied per request rather than stored for reuse, which leaves a single affected case: a bloom parked on a question at the moment the new binary boots needs its answer re-signed against the question digest.

## Consequences

- A captured envelope authorizes the one request it was made for. Re-parenting it produces a different artifact address and no verifying signature.
- The answer door gains a binding it never had. Its safety stops depending on operators writing distinct answer text.
- The orphan-claim release door lands bound rather than landing exposed and being retrofitted, provided this decision sequences ahead of ADR-0179's implementation.
- `POST /blooms/{id}/answer` changes shape to name its question. Pre-1.0, and the guide route table changes with it.
- Every signed envelope in existence stops verifying at the cutover, by design.
- The closed `AuthorityDoor` enum is a small tax on each new signed door and the mechanism that makes forgetting the binding impossible rather than merely discouraged.
- Key custody, rotation, and revocation stay where ADR-0149 and ADR-0151 leave them. This decision changes what is signed, never who may sign.

## Alternatives considered

- **Add the request digest as a field on `Statement`** — the obvious answer, and the wire codec forbids it: positional struct decode with no field presence makes every persisted `Fact::AdoptAnswer` undecodable, stranding a mid-flight coordinator at replay.
- **Version the `Statement` shape** — a parallel `StatementV2` and parallel `Fact` variants keep old journals readable at the cost of a permanent second vocabulary that every door, guide, and adapter must then carry.
- **Give each door a words template that embeds the request digest** — what the approve door already does, and it works; it stays a per-door convention that the compiler cannot enforce, so the next door repeats this defect by omission. It also forces the answer door to prefix a digest onto the text a person reads and signs.
- **Accept legacy envelopes during a migration window** — the window admits precisely the captured envelopes the change exists to reject.
- **Remove `parents` once the binding is signed** — a shape change with the same replay break, and it contradicts ADR-0149's rule that every artifact names its parents; the reducer also needs a key-free structural check it can evaluate on replay.
- **Bind the answer to its bloom rather than its question** — it avoids the route change and leaves one signed answer replayable across every open hold in that bloom.
- **Amend ADR-0149 in place instead of writing this ADR** — its prior in-place amendments record refinements that had one available answer; this decision has alternatives with real tradeoffs and a closed door enum later ADRs will extend, which need their own Decision and Alternatives sections. ADR-0149 §The value vocabulary gains a pointer to this ADR instead.
