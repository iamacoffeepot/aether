export const meta = {
  name: 'review',
  description: "Pre-land review of a code change against five judgment pillars that mechanical gates (clippy/rustc/Qodana/fmt) cannot decide: spec fidelity (asked-vs-changed delta), correctness (named bug-shapes), test integrity (does the test catch an owned bug), economy (fewest chars that still make sense), and convention/architecture (stated rules + ADR conformance, fed back as lint candidates). Cheap whole-PR scope filter first, then per-file specialist finders, then a two-sided verify funnel (refute low/med findings, challenge clean lenses). Returns an issue-ready rollup with soft-hold flags on high-severity correctness/spec findings; never gates CI, never touches GitHub.",
  whenToUse: "At the end of /implement (before un-draft) to review a change against its issue, or in backfill mode (no issue, whole-file, sharded per-crate) to audit existing code. The caller resolves the file set and passes it in args — the workflow sandbox cannot run git/grep itself. Output is a triage rollup for human review; filing follow-up issues and clearing soft-holds are separate human-gated steps.",
  phases: [
    { title: 'Scope', detail: 'one agent over the issue + whole diff: out-of-scope prune + over/under-delivery/leakage findings' },
    { title: 'Find', detail: 'per-file specialist finders, one per applicable pillar lens' },
    { title: 'Verify', detail: 'refute low/med-confidence findings (false positives) and challenge clean lenses (false negatives)' },
  ],
}

// review — merge of audit-tests + audit-quality into one pre-land review.
//
// args = {
//   files,        [string]            — ABSOLUTE paths to run the code lenses (economy,
//                                       correctness, convention) on (REQUIRED).
//   testFiles,    [string]            — ABSOLUTE paths to run test-integrity on (files with
//                                       #[test]/#[tokio::test]; caller greps for them). Optional;
//                                       omit to skip test review. May overlap with files.
//   issue,        string              — the issue/scope text. Presence enables Phase 0
//                                       (spec fidelity, integrated mode); omit for backfill.
//   diffs,        { [absPath]: text } — per-file diff hunks. Presence makes finders diff-scoped;
//                                       omit for whole-file review (backfill).
//   lenses,       [string]            — optional subset of pillar keys to run.
//   finderModel,  string              — model for Phase 0 + finders (default 'sonnet').
//   verifyModel,  string              — model for the verify funnel (default 'opus').
//   noChallenge,  bool                — skip the clean-lens challenge (cheaper).
// }
//
// returns { rollup, files }. rollup = { totals, spec, softHolds, confirmed, grouped,
// lintCandidates, spared, uncertain }. confirmed is issue-ready (each row tagged source:
// high-confidence | refuter | challenger-missed, and gate: soft-hold | advisory). softHolds
// is the subset a reviewer clears before un-draft. lintCandidates is the flywheel feed toward
// Layer 0 (clippy.toml / a custom lint / a check-*.sh rule).
//
// Backfill drives this once per crate (caller loop), e.g.
//   files=$(git ls-files 'crates/<crate>/src/**/*.rs')
//   testFiles=$(git grep -lE '#\[(test|tokio::test)\]' -- 'crates/<crate>/**/*.rs')

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const FILES = Array.isArray(A.files) ? A.files : []
const TEST_FILES = Array.isArray(A.testFiles) ? A.testFiles : []
if (!FILES.length && !TEST_FILES.length) throw new Error('review: args.files (and/or args.testFiles) must be a non-empty array of absolute .rs paths')

const FINDER_MODEL = A.finderModel || 'sonnet'
const VERIFY_MODEL = A.verifyModel || 'opus'
const DIFFS = A.diffs || {}

// Pillars where a high-confidence finding STILL gets a verifier — a confident hallucination
// does the most damage in correctness, so it is never trusted on confidence alone.
const ALWAYS_VERIFY = new Set(['correctness'])
// Pillars worth a clean-lens challenge (false negatives hide here). Economy/convention misses
// are low-stakes and the Opus all-lens challenge cost a third of a run for zero yield, so it is
// restricted and runs on the cheap model.
const CHALLENGE_LENSES = new Set(['correctness', 'test-integrity'])

const base = (f) => f.split('/').slice(-1)[0]

const NORTH_STAR = `NORTH STAR (economy): "the fewest characters that still make sense." OVER-VERBOSE = a strictly shorter form reads at least as clearly and loses no meaning, safety, or exhaustiveness. OVER-TERSE = the code is compressed past clarity/safety and the fix ADDS characters to restore them. Ceremony (a long name, a confident abstraction, a clever one-liner) is not evidence — flag only when you can state the concrete better form AND why it is at least as clear/correct. When unsure, do not flag.`

const BAR = `The bar for keeping a finding (strict, all lenses): the fix must be strictly better, not merely different.
- ECONOMY: the suggested form is "the fewest characters that still make sense" — shorter AND at least as clear/safe (over-verbose), or the terse form genuinely costs clarity/safety so the longer fix is worth it (over-terse). Reject taste ("I would write it differently").
- CORRECTNESS: name the concrete input or code path that misbehaves. Reject "could be unsafe" with no path; reject a hazard rustc/borrowck already prevents.
- CONVENTION: cite the CLAUDE.md / ADR rule it breaks. Reject anything Layer 0 (clippy -D warnings, fmt, Qodana, check-no-dividers, the disallowed-methods bans) already gates — that is a lintCandidate, not a judgment finding.
- TEST-INTEGRITY: junk unless the test exercises owned logic a plausible edit to THIS crate would break (and that the shared derive/codec/registry machinery's own tests would NOT already catch), OR it pins a COMPUTED value (a hash, golden bytes, a derived KindId number). A symmetric derive-only roundtrip, a name/schema-shape mirror, or a registry re-test is junk however much ceremony surrounds it.
Policy anchors: CLAUDE.md and docs/guide/testing.md.`

// The five pillars. unit drives the fan-out shape: 'whole-PR' runs once in Phase 0;
// 'file' fans out per changed file; 'test-file' runs only on testFiles. gate is the
// pillar's maximum severity weight: 'soft-hold' findings can hold the land at high
// severity, 'advisory' never block.
const ALL_LENSES = [
  {
    key: 'spec-fidelity',
    name: 'Spec fidelity',
    oracle: 'the issue / scope text',
    unit: 'whole-PR',
    gate: 'soft-hold',
    taxonomy: `The delta between what the issue asked for and what the diff changed.
- OVER-DELIVERY: code beyond the ask — a speculative abstraction, a config knob, an error variant, a helper/trait, or generality nobody scoped. The engine builds the algorithm for THIS problem, not every future one.
- UNDER-DELIVERY: the diff stops short — a TODO/unimplemented/stub on a scoped path, a stated acceptance criterion unmet, an error case the scope named left unhandled.
- SCOPE LEAKAGE: files or symbols the issue never mentions — drive-by reformatting, an unrelated refactor, a rename outside the stated scope.
- SILENT DEVIATION: the diff solves the problem a different way than the scope specified without saying so. A better idea is a question, not a quiet substitution.`,
    carveOut: `Needs the issue text; in backfill mode (no issue) this lens does not run. Judge ONLY the asked-vs-changed delta. In-scope-but-ugly is economy's; in-scope-but-buggy is correctness's; in-scope-but-rule-breaking is convention's.`,
  },
  {
    key: 'correctness',
    name: 'Correctness',
    oracle: "the code's own contract — what it implies it should do",
    unit: 'file',
    gate: 'soft-hold',
    taxonomy: `Named bug-shapes (NOT "find any bug" — flag only these, each with a concrete misbehaving input/path).
- SWALLOWED ERROR: a fallible result dropped — let _ = on a Result, .ok() discarding an Err, unwrap()/expect() on a runtime-fallible path, an omitted ? that loses an error, an error remapped to a less-informative one.
- MISSING BOUNDS CAP: recursion or geometrically-/user-derived iteration without the CLAUDE.md-mandated depth/budget cap that returns an error rather than overflowing; unbounded growth; integer overflow on user-derived arithmetic.
- SILENT INCOMPLETENESS: TODO / todo!() / unimplemented!() / a "for now" stub / a branch or match arm that no-ops where the logic is required.
- INVARIANT VIOLATION: new code that can put a type into a state its invariant forbids (flag the violation; the over-broad pub exposing the field is economy/visibility).
- RESOURCE LEAK: a handle / subscription / mixer-lane / texture / spawned child acquired without the matching release on every path, including early-return and error paths.
- CONCURRENCY: a data race / lost update / lock-order hazard. SCOPED: actor state is single-threaded behind its run-token (ADR-0038), so this shape is N/A in actor/component code — apply it ONLY in aether-substrate / native chassis code.`,
    carveOut: `Judgment about behavior, not lints. rustc owns type/borrow safety; where a clippy lint already fires, route it to lintCandidates. Do not flag style/verbosity (economy) or wrong-feature (spec). A finding must name the concrete input or path that misbehaves.`,
  },
  {
    key: 'test-integrity',
    name: 'Test integrity',
    oracle: 'the testing policy (docs/guide/testing.md) — what owned logic the test exercises',
    unit: 'test-file',
    gate: 'advisory',
    taxonomy: `THE DECISIVE QUESTION: what logic owned by THIS crate does the test exercise? If the honest answer is "none — it restates a declaration or re-runs machinery another crate owns", it is JUNK however much ceremony surrounds it. Junk shapes: mirror (incl. derived-constant: assert_eq!(K::NAME, "literal")), derive-only-roundtrip (symmetric decode(encode(x))==x over plain #[derive]s), not-owned (std/serde/wgpu/the Kind/Schema/Config derives), re-tests-machinery (descriptors::all() membership, SchemaType-shape asserts, config resolution, id/lineage hashing), mock-theater, no-assertion/echo, vacuous, bulk-dup, coverage-chasing.
TRIPWIRE (the only flat-value keep): the pinned value is COMPUTED — a hash, golden bytes, a derived KindId number — so it drifts when the producing LOGIC changes. A name pinned against its own #[kind(name)] literal or a SchemaType keyword restatement is a mirror, not a tripwire, even with a // Tripwire: comment.`,
    carveOut: `Only #[test]/#[tokio::test] fns. The non-test code the test drives is correctness/economy's. Read the full policy at docs/guide/testing.md — this taxonomy is its summary. Express a junk test as a finding: recommendation 'remove' (or 'rewrite'), category = the junk shape, current_form = the test signature.`,
  },
  {
    key: 'economy',
    name: 'Economy',
    oracle: 'the fewest characters that still make sense',
    unit: 'file',
    gate: 'advisory',
    taxonomy: `Both directions (see NORTH STAR). Sub-lenses — judgment only; the rule-based halves are convention's, the primitive-exists halves route to lintCandidates:
- NAMING: a name restating its context (config.config_path -> .path); a lying conversion prefix (as_ that allocates, to_ that only borrows, into_ that does not consume — C-CONV); one concept named two ways (count/len/size). [units/type-in-name/generics are convention's]
- OWNERSHIP/INDIRECTION: Arc/Rc/Box/clone heavier than the sharing is real; clone-to-satisfy-borrowck; a clone of a Copy value. OVER-TERSE counter: a hand-rolled cell where Rc/Arc is the honest primitive. [a lock/cell in actor state is convention's rule]
- STRUCTURE: a god-module accreting unrelated responsibilities (name them); a helper far from its sole caller, a type upstream of its only consumer; long inline ::-paths a use import reads better. [dividers + suffix-siblings are convention's]
- VISIBILITY: pub broader than where it is referenced (-> pub(crate)/pub(super)/private); an invariant-bearing pub field (-> private + ctor, C-STRUCT-PRIVATE); an impl type leaked through a public signature. OVER-TERSE counter: a getter/setter pair over a field with NO invariant. [unreachable_pub/dead_code are Layer 0]
- CONTROL-FLOW: OVER-VERBOSE — a manual accumulate loop a map/filter/collect states clearer, rightward drift let-else flattens, a match on Option/Result that ?/map_or/ok_or shortens. OVER-TERSE — an if-let or _ => () dropping exhaustiveness a future variant should force; a combinator chain past readability; a one-liner hiding a side effect. [clippy's style/complexity rewrites are Layer 0]
- TYPE-DESIGN: primitive obsession (adjacent same-type ids; a bare f32 that is really Seconds vs Pixels -> newtype, C-NEWTYPE); a bool param that should be a 2-variant enum (C-CUSTOM-TYPE); a stringly-typed mode -> enum. OVER-TERSE counter: a newtype with no invariant or static distinction (pure ceremony).`,
    carveOut: `Anything a clippy/rustc lint already decides -> lintCandidates. Rule-based naming/structure -> the convention lens. A hand-rolled existing primitive (geometry in aether-math, a re-implemented container, a hand-hashed address where the typed ctx.actor::<Cap>() resolver belongs) -> lintCandidates (reuse is a future pillar). Fill direction (over-verbose|over-terse) and char_delta on every economy finding.`,
  },
  {
    key: 'convention',
    name: 'Convention & architecture',
    oracle: 'CLAUDE.md conventions + ADRs',
    unit: 'file',
    gate: 'advisory',
    taxonomy: `Stated rules an agent reverts away from (world-idioms over repo-idioms), and ADR conformance. EVERY finding here is also a lint candidate.
- UNITS: a unit abbreviated to two letters (ms/ns/us/kb -> millis/nanos/micros/bytes).
- TYPE-IN-NAME: u32/u64/usize encoded in an identifier (parse_u32_millis -> parse_millis).
- GENERICS: a multi-letter generic param reading as a type alias (Ctx/KindT -> C/K).
- DRIVER NAMING: a passive *Capability that is actually a driver -> *DriverCapability.
- MODULE SIBLINGS: suffix files foo_x.rs/foo_y.rs -> a parent dir foo/{mod,x,y}.rs.
- ACTOR STATE: a Mutex/RwLock/RefCell/Cell/atomic in an aether actor's state (ADR-0038: actor state is plain fields behind the run-token).
- ADR CONFORMANCE (judgment): a cross-actor path that is not mail; a native/wasm boundary the substrate/actor split forbids; an addressing pattern outside the lineage model (ADR-0099) not yet clippy-banned.`,
    carveOut: `Do not re-judge Layer 0 (clippy -D warnings, fmt, Qodana, check-no-dividers, the env::var / mailbox_id_from_name disallowed-methods). If a rule is ALREADY gated, it cannot appear in a clean diff — finding one means the gate has a HOLE; emit it as a lintCandidate with note 'gate-gap'. Pure code-quality judgment with no stated rule -> economy.`,
  },
]

const SELECTED = Array.isArray(A.lenses) && A.lenses.length
  ? ALL_LENSES.filter(l => A.lenses.includes(l.key))
  : ALL_LENSES
if (!SELECTED.length) throw new Error(`review: no lenses selected; valid keys are ${ALL_LENSES.map(l => l.key).join(', ')}`)

const SPEC_LENS = SELECTED.find(l => l.key === 'spec-fidelity')
const FILE_LENSES = SELECTED.filter(l => l.unit === 'file')
const TEST_LENS = SELECTED.find(l => l.unit === 'test-file')

// Deterministic scope resolution (no agents): one entry per file, carrying the lenses that
// apply to it (code lenses for files, test-integrity for testFiles) and its diff hunk if any.
function resolveScope() {
  const all = [...new Set([...FILES, ...TEST_FILES])]
  return all.map(f => {
    const lenses = []
    if (FILES.includes(f)) lenses.push(...FILE_LENSES)
    if (TEST_FILES.includes(f) && TEST_LENS) lenses.push(TEST_LENS)
    return { file: f, lenses, diff: DIFFS[f] || null }
  }).filter(e => e.lenses.length)
}

const SPEC_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['outOfScope', 'findings'],
  properties: {
    outOfScope: { type: 'array', description: 'absolute paths of changed files the issue never asked to touch — pruned from the per-file passes; each also appears below as a scope-leakage finding', items: { type: 'string' } },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['category', 'file', 'symbol', 'description', 'severity', 'confidence'],
        properties: {
          category: { type: 'string', enum: ['over-delivery', 'under-delivery', 'scope-leakage', 'silent-deviation'] },
          file: { type: 'string', description: 'the file the finding is about, or "" for a PR-wide observation' },
          symbol: { type: 'string', description: 'the added/missing item, or the unmet acceptance criterion' },
          description: { type: 'string', description: 'the delta: what the issue asked, what the diff did, why they differ' },
          severity: { type: 'string', enum: ['high', 'medium', 'low'] },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
      },
    },
  },
}

const FINDING_ITEM = {
  type: 'object',
  additionalProperties: false,
  required: ['symbol', 'line', 'category', 'severity', 'confidence', 'recommendation', 'current_form', 'suggested_form', 'rationale'],
  properties: {
    symbol: { type: 'string', description: 'the item — fn/struct/field/binding/test name + a short locator' },
    line: { type: 'integer', description: 'approximate line of the site (advisory)' },
    category: { type: 'string', description: 'the lens sub-shape (e.g. economy naming|ownership|control-flow; correctness swallowed-error|missing-bounds-cap; convention units|generics; test-integrity mirror|derive-only-roundtrip)' },
    severity: { type: 'string', enum: ['high', 'medium', 'low'], description: 'impact if unaddressed — correctness/spec high-severity soft-holds the land' },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'], description: 'low/medium routes to a refuter; high goes straight to the rollup' },
    recommendation: { type: 'string', enum: ['fix', 'remove', 'rewrite', 'promote-lint'] },
    current_form: { type: 'string', description: 'the current code / name / test, briefly' },
    suggested_form: { type: 'string', description: 'the proposed action — code for economy/convention; the failing input + correct behavior for correctness; "remove" or the rewrite for test-integrity' },
    rationale: { type: 'string', description: 'the judgment: why the fix is at least as clear/correct/safe — not a restatement of the code' },
    direction: { type: 'string', enum: ['over-verbose', 'over-terse'], description: 'ECONOMY lens only' },
    char_delta: { type: 'integer', description: 'ECONOMY lens only: suggested length minus current length, in characters (negative = shorter)' },
  },
}

const FIND_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['file', 'lens', 'findings', 'lintCandidates'],
  properties: {
    file: { type: 'string' },
    lens: { type: 'string' },
    findings: { type: 'array', items: FINDING_ITEM },
    lintCandidates: {
      type: 'array',
      description: 'mechanically-decidable observations (the lens carve-out) — seed for clippy.toml / a custom lint / a check-*.sh rule, NOT part of the judgment rollup',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['symbol', 'note'],
        properties: {
          symbol: { type: 'string' },
          note: { type: 'string', description: "the mechanical rule that covers it (clippy::single_match, non_snake_case, check-no-dividers, disallowed-methods:env::var, …), or 'gate-gap' if the rule is ALREADY gated and slipped through" },
        },
      },
    },
  },
}

const VERDICT = {
  type: 'object',
  additionalProperties: false,
  required: ['symbol', 'final_verdict', 'rationale'],
  properties: {
    symbol: { type: 'string' },
    final_verdict: { type: 'string', enum: ['confirmed', 'false-positive', 'uncertain'] },
    rationale: { type: 'string', description: 'why the fix genuinely wins under the strict bar, OR why it is a false positive; use uncertain only when the relevant code could not be read' },
  },
}

const CHALLENGE = {
  type: 'object',
  additionalProperties: false,
  required: ['lens', 'final_verdict', 'missed'],
  properties: {
    lens: { type: 'string' },
    final_verdict: { type: 'string', enum: ['clean-confirmed', 'missed', 'uncertain'] },
    missed: { type: 'array', description: 'findings the finder overlooked (empty unless final_verdict = missed)', items: FINDING_ITEM },
  },
}

function specPrompt(issue, scoped) {
  const fileList = scoped.map(f => f.diff ? `--- ${f.file}\n${f.diff}` : f.file).join('\n\n')
  const diffed = scoped.some(f => f.diff)
  return `You are reviewing a code change for SPEC FIDELITY — the delta between what an issue asked for and what the diff actually changed. Read the issue, then the changed files${diffed ? ' (diff hunks shown)' : ''} (open the full files where you need context).

ISSUE / SCOPE:
${issue}

CHANGED FILES:
${fileList}

${SPEC_LENS.taxonomy}

${SPEC_LENS.carveOut}

Report outOfScope = the files the issue never asked to touch (drive-by / unrelated — they will be pruned from the deeper passes). findings = each over-delivery / under-delivery / scope-leakage / silent-deviation with the file, the item, the asked-vs-changed delta, severity, and confidence. If the change faithfully matches the issue, return empty arrays. Be conservative: a refactor the issue implies is not leakage; flag only a genuine mismatch.`
}

function findPrompt(f, lens) {
  const scope = f.diff
    ? `Focus on the CHANGED lines in the diff below; open the full file at ${f.file} for context, and scope every finding to the change.\n\nDIFF:\n${f.diff}`
    : `Read the full file at ${f.file} (and the types/helpers it references where you need them to judge).`
  const north = lens.key === 'economy' ? `\n${NORTH_STAR}\n` : ''
  return `You are one specialist judge on a code-review panel, auditing a Rust file under a single lens.

Lens: ${lens.name}
Oracle (what you judge against): ${lens.oracle}
${lens.taxonomy}

CARVE-OUT (do NOT raise as judgment findings — route to lintCandidates or the named pillar): ${lens.carveOut}
${north}
${scope}

Report every site this lens flags as a finding: symbol + approximate line, category (the sub-shape), severity, the current form, the suggested fix, a rationale stating the judgment (why the fix is at least as clear/correct/safe — not a restatement), and your confidence.${lens.key === 'economy' ? ' Fill direction (over-verbose|over-terse) and char_delta on every finding.' : ''} Put mechanically-decidable observations in lintCandidates, not findings. Report nothing this lens does not own — another panelist covers the others. Report ONLY sites located in ${f.file} itself: if you notice an issue in a different file it references, do not report it (that file gets its own finder). If the file is clean under this lens, return an empty findings array. Be precise and conservative: a confident story is not a finding; flag only when you can name the concrete better form AND why it wins.`
}

function refutePrompt(f, fd) {
  const corr = fd.lens.key === 'correctness'
  const grounding = corr
    ? `\nGROUND THE VERDICT IN TESTS, not inspection — a confident reading of subtle code (math conventions, sign order, edge cases) is exactly where this lens hallucinates a bug.
1. Read the existing #[test]s for this item. A passing test that pins the claimed-broken behavior REFUTES the finding (final_verdict='false-positive') — cite the test by name.
2. If no test covers the claim and the finding is high-severity, WRITE one and RUN it: add a focused #[test], run \`cargo test -p <crate> <name>\` from the crate root, let the result decide, then delete the scratch test.
3. NEVER uphold a finding whose suggested fix would break a currently-passing test — check the fix against the suite before confirming.\n`
    : ''
  return `A code-review finding was raised under the ${fd.lens.name} lens. Decide whether it survives a STRICT bar — do not rescue it with a plausible story, and do not reject a real issue out of conservatism.

File: ${f.file}
Site: ${fd.symbol} (around line ${fd.line})
Category: ${fd.category}
Current form: ${fd.current_form}
Suggested fix: ${fd.suggested_form}
Finder rationale: ${fd.rationale}

Read the site and the code it depends on.
${grounding}
${BAR}

If the finding genuinely meets the bar, final_verdict='confirmed' with the concrete reason it holds (for correctness: the failing input/path, confirmed against a test where possible). If the code is fine as written (the verbosity/terseness is load-bearing, the behavior is correct, a passing test already pins it, the rule does not apply), final_verdict='false-positive'. Use 'uncertain' only when you cannot read or run the relevant code.`
}

function challengePrompt(f, lens) {
  return `The ${lens.name} lens reported NO findings for this file. CHALLENGE that clean verdict — re-read the file and look specifically for what this lens catches that a first pass missed.

File: ${f.file}
Oracle: ${lens.oracle}
${lens.taxonomy}

CARVE-OUT (do NOT raise — mechanically settled or another pillar's): ${lens.carveOut}

${BAR}

If the file is genuinely clean under this lens, final_verdict='clean-confirmed' and missed=[]. If you find real issues that meet the bar, final_verdict='missed' and list them in missed[] using the full finding shape (symbol, line, category, severity, confidence, recommendation, current_form, suggested_form, rationale). Use 'uncertain' only when you cannot read the relevant code.`
}

const scoped = resolveScope()
if (!scoped.length) throw new Error('review: no files resolved to any lens (check files/testFiles vs selected lenses)')

// Phase 0 — spec-fidelity scope filter (integrated mode only). Barrier: it reads the whole
// diff to decide per-file scope, and its verdict prunes the per-file passes, so they wait.
let spec = null
let inScope = scoped
if (A.issue && SPEC_LENS) {
  phase('Scope')
  spec = await agent(specPrompt(A.issue, scoped), { label: 'spec-fidelity', phase: 'Scope', model: FINDER_MODEL, schema: SPEC_SCHEMA })
  const out = new Set((spec && spec.outOfScope) || [])
  inScope = scoped.filter(f => !out.has(f.file))
  if (out.size) log(`spec-fidelity: ${out.size} out-of-scope file(s) pruned from the per-file passes`)
}

// Phases 1+2 — per-file funnel, pipelined (no barrier between files). Stage 1 fans out the
// applicable specialist finders; stage 2 refutes low/med findings and challenges clean lenses.
const results = await pipeline(
  inScope,
  (f) => parallel(f.lenses.map(lens => () =>
    agent(findPrompt(f, lens), { label: `find:${lens.key}:${base(f.file)}`, phase: 'Find', model: FINDER_MODEL, schema: FIND_SCHEMA })
      .then(r => ({ lens, r }))
  )),
  async (lensRuns, f) => {
    const runs = (lensRuns || []).filter(Boolean).filter(x => x.r)
    if (!runs.length) return { file: f.file, runs: [], verified: [], challenged: [] }

    const flags = []
    for (const x of runs) for (const fd of (x.r.findings || [])) flags.push({ ...fd, lens: x.lens })
    // Correctness is verified regardless of confidence; other lenses skip high-confidence flags.
    const toRefute = flags.filter(fd => ALWAYS_VERIFY.has(fd.lens.key) || fd.confidence !== 'high')
    const refute = parallel(toRefute.map(fd => () =>
      agent(refutePrompt(f, fd), {
        label: `refute:${fd.lens.key}:${fd.symbol}`.slice(0, 80),
        phase: 'Verify',
        model: VERIFY_MODEL,
        // Correctness gets a Bash-capable agent so it can run a grounding test, not just read.
        agentType: fd.lens.key === 'correctness' ? 'general-purpose' : undefined,
        schema: VERDICT,
      }).then(v => ({ lensKey: fd.lens.key, symbol: fd.symbol, verify: v }))
    ))

    // Challenge only the pillars where misses hide, on the cheap model.
    const cleanRuns = A.noChallenge ? [] : runs.filter(x => !(x.r.findings || []).length && CHALLENGE_LENSES.has(x.lens.key))
    const challenge = parallel(cleanRuns.map(x => () =>
      agent(challengePrompt(f, x.lens), { label: `challenge:${x.lens.key}:${base(f.file)}`, phase: 'Verify', model: FINDER_MODEL, schema: CHALLENGE })
        .then(c => ({ lens: x.lens, challenge: c }))
    ))

    return { file: f.file, runs, verified: (await refute).filter(Boolean), challenged: (await challenge).filter(Boolean) }
  }
)

// Deterministic rollup. A finding is CONFIRMED three ways: high-confidence (no refuter),
// refuter 'confirmed' (a low/med flag upheld), or challenger 'missed' (a clean lens overlooked
// it). 'spared' = a flag the refuter overturned. gate = soft-hold only for a high-severity
// finding on a soft-hold pillar (spec/correctness); everything else advisory.
const gateFor = (lens, severity) => (lens.gate === 'soft-hold' && severity === 'high') ? 'soft-hold' : 'advisory'
const rowOf = (file, lens, fd, source) => ({
  file, pillar: lens.key, source, category: fd.category, line: fd.line, symbol: fd.symbol,
  severity: fd.severity, gate: gateFor(lens, fd.severity), recommendation: fd.recommendation,
  suggested_form: fd.suggested_form, direction: fd.direction, char_delta: fd.char_delta,
})

const clean = results.filter(Boolean)
const totals = { files: 0, finders: 0, findings: 0, confirmed: 0, falsePositives: 0, challengerMissed: 0, softHolds: 0 }
const confirmed = [], spared = [], uncertain = [], lintCandidates = []

for (const e of clean) {
  if (!e.runs || !e.runs.length) continue
  totals.files++
  const file = base(e.file)
  const vmap = new Map((e.verified || []).map(v => [`${v.lensKey}:${v.symbol}`, v.verify]))

  for (const x of e.runs) {
    totals.finders++
    for (const lc of (x.r.lintCandidates || [])) lintCandidates.push({ file, lens: x.lens.key, symbol: lc.symbol, note: lc.note })
    for (const fd of (x.r.findings || [])) {
      totals.findings++
      if (fd.confidence === 'high') confirmed.push(rowOf(file, x.lens, fd, 'high-confidence'))
      else {
        const v = vmap.get(`${x.lens.key}:${fd.symbol}`)
        if (v && v.final_verdict === 'confirmed') confirmed.push(rowOf(file, x.lens, fd, 'refuter'))
        else if (v && v.final_verdict === 'false-positive') { spared.push({ ...rowOf(file, x.lens, fd, 'spared'), reason: v.rationale }); totals.falsePositives++ }
        else uncertain.push({ ...rowOf(file, x.lens, fd, 'uncertain'), stage: 'refute', note: (v && v.rationale) || 'no verify result' })
      }
    }
  }

  for (const ch of (e.challenged || [])) {
    const v = ch.challenge
    if (v && v.final_verdict === 'missed') {
      for (const m of (v.missed || [])) { totals.challengerMissed++; confirmed.push(rowOf(file, ch.lens, m, 'challenger-missed')) }
    } else if (v && v.final_verdict === 'uncertain') {
      uncertain.push({ file, pillar: ch.lens.key, symbol: '(clean-lens challenge)', stage: 'challenge', note: 'challenger could not read the relevant code' })
    }
  }
}

// Dedup backstop: a finder that strayed into another file, or two sub-lenses overlapping,
// can raise the same site twice (the in-file finder constraint prevents most of it). Keep the
// first row per (file, pillar, category, line).
const seenRows = new Set()
const deduped = []
for (const r of confirmed) {
  const k = `${r.file}:${r.pillar}:${r.category}:${r.line}`
  if (seenRows.has(k)) continue
  seenRows.add(k); deduped.push(r)
}

totals.confirmed = deduped.length
const softHolds = deduped.filter(r => r.gate === 'soft-hold')
totals.softHolds = softHolds.length

// grouped: by file then pillar, so the rollup drops straight into /sketch.
const grouped = {}
for (const r of deduped) ((grouped[r.file] ||= {})[r.pillar] ||= []).push(r)
const bySource = deduped.reduce((a, r) => ((a[r.source] = (a[r.source] || 0) + 1), a), {})
const specCount = spec ? (spec.findings || []).length : 0

log(`review: ${totals.files} files, ${totals.finders} finders -> ${totals.confirmed} confirmed (high-conf ${bySource['high-confidence'] || 0}, refuter ${bySource['refuter'] || 0}, challenger ${bySource['challenger-missed'] || 0}), ${softHolds.length} SOFT-HOLD, ${spared.length} spared, ${uncertain.length} uncertain, ${lintCandidates.length} lint candidates, ${specCount} spec findings`)

return {
  rollup: {
    totals,
    spec: spec ? { findings: spec.findings || [], outOfScope: spec.outOfScope || [] } : null,
    softHolds, confirmed: deduped, grouped, lintCandidates, spared, uncertain,
  },
  files: clean,
}
