export const meta = {
  name: 'audit-tests',
  description: "Audit a crate's tests against the testing policy (docs/guide/testing.md): classify every #[test] by what logic the crate actually owns, then adversarially verify junk verdicts and challenge high-risk keeps under a strict bar. Returns an issue-ready rollup of tests to remove.",
  whenToUse: 'When auditing a crate (or file set) for junk tests — tests that restate declarations, re-run shared derive/codec/registry machinery, or duplicate a sibling. Scope the file list first (grep the crate for #[test]) and pass absolute paths. Output is a triage rollup for human review; filing the removal issue is a separate /sketch step.',
  phases: [
    { title: 'Classify', detail: 'one agent per test file, classify each test fn against the policy' },
    { title: 'Verify', detail: 'refute junk verdicts and challenge high-risk keeps under the strict bar' },
  ],
}

// audit-tests — audit a crate's tests against docs/guide/testing.md.
//
// Two phases. CLASSIFY: one agent per test file judges every #[test] against the
// decisive question — what logic owned by THIS crate does the test exercise? VERIFY
// is two-sided: a refuter re-checks low/medium-confidence JUNK verdicts (guards false
// positives), and a keep-challenger re-judges high-risk KEEPs (guards false negatives,
// the shape a single classifier pass tends to miss). The challenger seeds on a prose
// regex, then if any keep in a file flips to junk it challenges every remaining keep in
// that file — structural-shape contagion, so a same-shape sibling is not missed for
// lack of a matched keyword.
//
// args = {
//   files,   [string]  — ABSOLUTE paths to the test files to audit (REQUIRED).
//   policy,  string    — path to the testing policy each agent reads
//                        (default 'docs/guide/testing.md', resolved from the agent cwd).
// }
//
// returns { rollup, files }, where rollup = { totals, confirmedJunk, grouped, spared,
// challengerFlips, uncertain }. confirmedJunk is issue-ready — each row tagged by how it
// was confirmed (high-confidence | refuter | challenger-flip); grouped is by file so it
// drops straight into /sketch. A challenger-flip overturns a 'load-bearing' verdict, so
// those are the judgment calls a reviewer should eyeball before filing.
//
// Scope the file list yourself before invoking, e.g.
//   grep -rlE '#\[(test|tokio::test)\]' crates/<crate> --include='*.rs'
// Filing the removal issue is a human-gated step AFTER review, via /sketch — this
// workflow finds and classifies; it never touches GitHub.

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const POLICY = A.policy || 'docs/guide/testing.md'
const FILES = Array.isArray(A.files) ? A.files : []
if (!FILES.length) throw new Error('audit-tests: args.files must be a non-empty array of absolute test-file paths')

const TAXONOMY = `THE DECISIVE QUESTION for every test: what logic owned by THIS crate (the crate the test file lives in) does it exercise? A test is JUNK if the only honest answer is "none — it restates a declaration, or re-runs machinery another crate owns and tests" — no matter how much ceremony (field-by-field asserts, a large constructed value, a confident doc comment) surrounds it. Ceremony is camouflage, not evidence of load. Junk categories:
- mirror: restates the source as an assertion. Includes the DERIVED-CONSTANT mirror — assert_eq!(NoteOn::NAME, "aether.audio.note_on") where NAME IS the #[kind(name="…")] literal: the expected value is the same string retyped, no independent source of truth. A rename edits the attribute and the adjacent test together; consumers route on NoteOn::NAME or its hash, so they track renames for free. Also assert_eq!(Foo::default().x, 0) next to x: 0.
- derive-only-roundtrip: decode(encode(x)) == x over a type whose Serialize/Deserialize/Schema are all #[derive]d. This is SYMMETRIC — encode and decode are generated from the same definition, so any change moves both in lockstep and the test still passes. It can only fail if the two DISAGREE, i.e. the derive macro is broken (tested where the macro lives). Building an elaborate value + asserting each field survives does NOT change this. Legitimate ONLY if the type has hand-written ser/de, OR the roundtrip exercises a real invariant (a clamp, a normalization, a rejected input). Plain derives over plain fields = junk.
- not-owned: tests code we do not own — std, the compiler (#[derive] output, Vec::len after pushes), serde, any third-party crate (wgpu/tokio/fontdue), AND anything the Kind/Schema/Config derives emit. The shared codec (encode_into_bytes/decode_from_bytes, wire::to_vec/from_bytes) is OURS but owned ONCE in aether-data — re-running it from a consumer crate on a consumer's struct tests the consumer's #[derive]s + the shared codec, neither of which is that crate's logic.
- re-tests-machinery: re-tests shared engine machinery from a consumer. Config #[derive(Config)] argv>env>default resolution; mail routing; settlement; id/lineage hashing; DERIVE-EMITTED REGISTRATION — asserting a kind is in descriptors::all() guards nothing because #[derive(Kind)] emits the inventory::submit! (nothing manual to forget; absence = broken derive or a compile error); SCHEMA-SHAPE assertions — matches!(Role::SCHEMA, SchemaType::Enum) just restates the enum/struct keyword the derive maps mechanically.
- mock-theater: only exercises mocks/fakes the test itself set up.
- no-assertion: calls the fn but never checks a result, asserts only "didn't panic", or checks output against a value recomputed the same way the code computes it (incl. asserting a string the TEST inserted comes back out — an echo, not an oracle).
- vacuous: assert!(true), empty body, zero-iteration loop, early return before the assertion, or a guard that skips on every machine.
- bulk-dup: many near-identical cases driving one branch with different literals where one table-driven case suffices.
- coverage-chasing: written to turn a line green — trivial getter, Display with no logic, unreachable match arm.

TRIPWIRE (the ONLY keep-exception for a flat assertion against a fixed value): KEEP only when the pinned value is COMPUTED — a hash, a serialized byte layout (golden bytes), a derived KindId numeric value — so it drifts when the LOGIC that produces it changes even though the declaration that named it did not. assert_eq!(NoteOn::ID, KindId(0x…)) is a tripwire (the id is hashed from the name). assert_eq!(NoteOn::NAME, "literal") is NOT (the name is the declaration restated). matches!(X::SCHEMA, Enum) is NOT (the shape is the keyword restated). A // Tripwire:/"golden"/"pin" comment or module doc is NECESSARY BUT NOT SUFFICIENT — a comment over a value that cannot drift on its own is a mirror with a story told over it, and stays junk. Do NOT default a hash/id/wire/name assertion to KEEP — first decide whether its value is COMPUTED or merely RESTATED.`

phase('Classify')

const CLASSIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['file', 'tests'],
  properties: {
    file: { type: 'string' },
    tests: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['name', 'line', 'verdict', 'category', 'rationale', 'recommendation', 'confidence'],
        properties: {
          name: { type: 'string' },
          line: { type: 'integer' },
          verdict: { type: 'string', enum: ['load-bearing', 'tripwire', 'junk'] },
          category: { type: 'string', description: 'junk category if verdict=junk, else "" or the reason it is load-bearing' },
          rationale: { type: 'string', description: 'for junk: why no plausible bug; for keep: the bug it catches' },
          recommendation: { type: 'string', enum: ['keep', 'remove', 'rewrite', 'add-tripwire-comment'] },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
      },
    },
  },
}

const VERIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['name', 'final_verdict', 'refutation'],
  properties: {
    name: { type: 'string' },
    final_verdict: { type: 'string', enum: ['junk-confirmed', 'spare', 'uncertain'] },
    refutation: { type: 'string', description: 'the plausible bug it catches / tripwire it is, OR confirmation none exists' },
  },
}

// Keep-challenger: re-judge a high-risk KEEP under the same strict bar. Guards the
// false-negative direction (junk the classifier rated keep/tripwire) the refuter
// cannot reach, since the refuter only runs on junk verdicts.
const CHALLENGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['name', 'final_verdict', 'rationale'],
  properties: {
    name: { type: 'string' },
    final_verdict: { type: 'string', enum: ['keep-confirmed', 'junk', 'uncertain'] },
    rationale: { type: 'string', description: 'the owned edit it catches / computed value it pins (keep), OR why neither holds (junk)' },
  },
}

// Round-1 seed for the keep-challenger: the shapes false negatives hide in — a tripwire
// verdict (looks-like-junk-but-kept), or a name/roundtrip/schema-shape/descriptor test.
// Prose-matched, so it can miss a same-shape sibling; the verify stage's round-2
// contagion (challenge the whole file once any seed flips) is what closes that gap.
const HIGH_RISK = /round.?trip|::NAME\b|_name|name.*stable|stable.*name|\bSCHEMA\b|schema.?shape|SchemaType|descriptor|inventory|registr|::ID\b|kind.?id/i
function isHighRiskKeep(t) {
  const keep = t.verdict !== 'junk' && t.recommendation !== 'remove'
  if (!keep) return false
  if (t.verdict === 'tripwire') return true
  return HIGH_RISK.test(`${t.name} ${t.category} ${t.rationale}`)
}

const results = await pipeline(
  FILES,
  (file) => agent(
    `You are auditing one Rust test file against a testing policy. Read the full policy at ${POLICY} and the test file at ${file}.\n\nTaxonomy:\n${TAXONOMY}\n\nFor EVERY #[test] / #[tokio::test] function in ${file}, apply THE DECISIVE QUESTION: what logic owned by THIS crate does the test exercise? Then classify:\n- 'load-bearing' (KEEP): exercises real owned logic — a branch, boundary, validation, computed value, hand-written ser/de — such that a plausible future edit to THIS crate's code breaks it.\n- 'tripwire' (KEEP): pins a COMPUTED value (hash / serialized byte layout / derived KindId number) against an independent constant. A name pinned against its own #[kind(name)] literal or a SchemaType shape that restates the type keyword is NOT a tripwire — it is a 'junk' mirror.\n- 'junk' (REMOVE): the honest answer to the decisive question is "none of ours" — a derived-constant mirror, a symmetric derive-only roundtrip, a derive-emitted registration/inventory check, a schema-shape restatement, or any re-run of machinery (derive/codec/registry) owned and tested in another crate. Ceremony (big values, field asserts, a confident doc comment) does NOT rescue it.\n\nDo NOT default to keep. A confident-sounding rationale or doc comment is not evidence — verify the test exercises owned logic or pins a computed value, or call it junk. recommendation: keep | remove (junk) | rewrite (tests something real but badly) | add-tripwire-comment (a real COMPUTED-value tripwire missing its comment). Give the exact line of the fn signature. Read helper code and the types under test (are the fields plain #[derive]s? is the value computed or declared?) before deciding.`,
    { label: `classify:${file.split('/').slice(-2).join('/')}`, phase: 'Classify', schema: CLASSIFY_SCHEMA }
  ),
  // Verify stage: two-sided. Refute each JUNK verdict (guards false positives), and
  // challenge each high-risk KEEP (guards false negatives — junk rated keep/tripwire).
  // Both apply the same strict bar; they differ only in which verdicts they re-judge.
  async (classified, file) => {
    if (!classified) return { file, classification: null, verified: [], challenged: [] }
    const BAR = `The bar (identical for both directions): a test is KEEP-worthy in ONLY two cases.\n1. OWNED-LOGIC: name ONE specific future edit to logic in THIS crate (the crate the test file lives in) — hand-written ser/de, a clamp, a validation, a branch, a computed value — that would break the test AND would NOT already be caught by the shared machinery's own tests (the Kind/Schema/Config derives, the codec in aether-data, the inventory registry). "The derive/codec could break" does NOT qualify — that is tested where it lives.\n2. COMPUTED-PIN: the test pins a COMPUTED value (a hash, a serialized byte layout / golden bytes, a derived KindId number) against an independent constant, so it drifts when the producing logic changes. A name pinned against its own #[kind(name)] literal, or a SchemaType::Enum/Struct shape that restates the type keyword, is NOT a computed pin — it is a mirror and stays junk even with a // Tripwire: comment.\nTraps to reject: decode(encode(x))==x over a derive-only type is SYMMETRIC (both sides move together) and catches nothing but a broken derive. Asserting a kind is in descriptors::all() re-tests #[derive(Kind)]'s emitted inventory::submit!. Asserting a string the test itself inserted comes back out is an echo, not an oracle.`

    // Refute only LOW/MEDIUM-confidence junk. A high-confidence junk verdict on a
    // mechanically-certain shape (symmetric derive roundtrip, derived-constant mirror)
    // is not a judgment call and does not need an adversary — across runs the refuter
    // confirmed 100% of these. The human reviews the final list regardless.
    const isJunk = (t) => t.verdict === 'junk' || t.recommendation === 'remove'
    const flagged = classified.tests.filter(t => isJunk(t) && t.confidence !== 'high')
    const allKeeps = classified.tests.filter(t => !isJunk(t))

    const refute = parallel(flagged.map(t => () =>
      agent(
        `A test was classified as JUNK. Decide whether it survives a STRICT bar — do not rescue it with a plausible story ("it pins the wire contract", "our codec is ours to test", "it proves the kind is registered"); reject those unless they meet the bar.\n\nTest: ${t.name} at ${file}:${t.line}\nClassifier category: ${t.category}\nClassifier rationale: ${t.rationale}\n\nRead the test (and surrounding helpers). Policy: ${POLICY}.\n\n${BAR}\n\nIf case 1 or 2 holds, final_verdict='spare' and refutation = the exact owned edit it catches or computed value it pins. Otherwise final_verdict='junk-confirmed'. Use 'uncertain' only when you cannot read the relevant logic.`,
        { label: `refute:${t.name}`, phase: 'Verify', schema: VERIFY_SCHEMA }
      ).then(v => ({ ...t, file, verify: v }))
    ))

    const challengeOne = (t) =>
      agent(
        `A test was classified as KEEP (verdict '${t.verdict}'). Your job is to CHALLENGE that — argue it is actually junk under the strict bar, and reject the classifier's keep-rationale unless it truly meets the bar. The classifier kept it; confirm that only if the bar is genuinely met, otherwise overturn it to junk.\n\nTest: ${t.name} at ${file}:${t.line}\nClassifier category: ${t.category}\nClassifier keep-rationale: ${t.rationale}\n\nRead the test (and surrounding helpers) AND the types under test — are the fields plain #[derive]s? is the asserted value computed, or just the declaration restated? Policy: ${POLICY}.\n\n${BAR}\n\nfinal_verdict='keep-confirmed' ONLY if case 1 or 2 genuinely holds (rationale = the exact owned edit it catches or computed value it pins). Otherwise final_verdict='junk' (rationale = why neither holds — name the declaration it restates or the machinery it re-runs). Use 'uncertain' only when you cannot read the relevant logic.`,
        { label: `challenge:${t.name}`, phase: 'Verify', schema: CHALLENGE_SCHEMA }
      ).then(v => ({ ...t, file, challenge: v }))

    // Challenge by structural shape, not classifier prose. Round 1 seeds on the
    // regex-matched high-risk keeps; if ANY of them flips to junk, the file is
    // suspect, so round 2 challenges every remaining keep in it — catching same-shape
    // siblings the prose seed missed (e.g. a Slot set/get test whose twin lacked the
    // matched keyword). The await is why this stage is async; refute runs concurrently.
    const seed = allKeeps.filter(isHighRiskKeep)
    const round1 = (await parallel(seed.map(t => () => challengeOne(t)))).filter(Boolean)
    let challenged = round1
    if (round1.some(c => c.challenge?.final_verdict === 'junk')) {
      const seedNames = new Set(seed.map(t => t.name))
      const rest = allKeeps.filter(t => !seedNames.has(t.name))
      const round2 = (await parallel(rest.map(t => () => challengeOne(t)))).filter(Boolean)
      challenged = round1.concat(round2)
    }

    const verified = (await refute).filter(Boolean)
    return { file, classification: classified, verified, challenged }
  }
)

// results[i] = { file, classification, verified, challenged }. classification carries
// every test's verdict; verified = refuted junk verdicts; challenged = re-judged
// high-risk keeps (final_verdict 'junk' = a false negative the classifier let through).
const clean = results.filter(Boolean)

// Deterministic rollup — reconcile classifier + verify + challenge into an
// issue-ready list, so the audit's output drops straight into /sketch. A junk
// verdict is CONFIRMED three ways: high-confidence (accepted without a refuter),
// refuter 'junk-confirmed' (low/med-confidence junk it upheld), or challenger
// 'junk' (a keep it overturned). 'spared' = junk the refuter rescued to keep.
const totals = { tests: 0, loadBearing: 0, tripwire: 0, junk: 0 }
const confirmedJunk = [], spared = [], uncertain = [], challengerFlips = []
for (const e of clean) {
  const c = e.classification
  if (!c) continue
  const base = e.file.split('/').slice(-1)[0]
  const verifyByName = new Map((e.verified || []).map(v => [v.name, v.verify]))
  const challengeByName = new Map((e.challenged || []).map(v => [v.name, v.challenge]))
  for (const t of c.tests) {
    totals.tests++
    if (t.verdict === 'load-bearing') totals.loadBearing++
    else if (t.verdict === 'tripwire') totals.tripwire++
    else if (t.verdict === 'junk') totals.junk++
    const row = { file: base, line: t.line, name: t.name, category: t.category }
    if (t.verdict === 'junk' || t.recommendation === 'remove') {
      if (t.confidence === 'high') confirmedJunk.push({ ...row, source: 'high-confidence' })
      else {
        const v = verifyByName.get(t.name)
        if (v?.final_verdict === 'junk-confirmed') confirmedJunk.push({ ...row, source: 'refuter' })
        else if (v?.final_verdict === 'spare') spared.push({ ...row, reason: v.refutation })
        else uncertain.push({ ...row, stage: 'refute', note: v?.refutation || 'no verify result' })
      }
    }
    const ch = challengeByName.get(t.name)
    if (ch?.final_verdict === 'junk') { confirmedJunk.push({ ...row, source: 'challenger-flip' }); challengerFlips.push({ ...row, reason: ch.rationale }) }
    else if (ch?.final_verdict === 'uncertain') uncertain.push({ ...row, stage: 'challenge', note: ch.rationale })
  }
}
const grouped = {}
for (const j of confirmedJunk) (grouped[j.file] ||= []).push(j)
const bySource = confirmedJunk.reduce((a, j) => ((a[j.source] = (a[j.source] || 0) + 1), a), {})
log(`audit-tests: ${totals.tests} tests → ${confirmedJunk.length} junk to remove (high-conf ${bySource['high-confidence'] || 0}, refuter ${bySource['refuter'] || 0}, challenger-flip ${bySource['challenger-flip'] || 0}), ${spared.length} spared, ${uncertain.length} uncertain, ${totals.loadBearing + totals.tripwire} keeps`)

return { rollup: { totals, confirmedJunk, grouped, spared, challengerFlips, uncertain }, files: clean }
