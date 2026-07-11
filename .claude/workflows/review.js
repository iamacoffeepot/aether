export const meta = {
  name: 'review',
  description: "Pre-land review of a code change against five judgment pillars that mechanical gates (clippy/rustc/Qodana/fmt) cannot decide: spec fidelity (asked-vs-changed delta), correctness (named bug-shapes), test integrity (does the test catch an owned bug), economy (fewest chars that still make sense, including large-file split pressure), and convention/architecture (stated rules + ADR conformance, fed back as lint candidates). Cheap whole-PR scope filter first, then per-file finders (a merged quality lens and batched small diffs in the pr profile), then a two-sided verify funnel (severity-tiered, per-file batched refutes plus a false-negative challenge). Returns an issue-ready rollup with soft-hold flags on high-severity correctness/spec findings; never gates CI, never touches GitHub.",
  whenToUse: "Integrated review of a PR-bound change is automatic in CI (the review action runs this workflow at the PR's first green) — do not invoke it inline at the end of /implement. Invoke for backfill mode (no issue, whole-file, sharded per-crate) to audit existing code, or for a change that never becomes a PR. The caller resolves the file set and passes it in args — the workflow sandbox cannot run git/grep itself. Output is a triage rollup for human review; filing follow-up issues and clearing soft-holds are separate human-gated steps.",
  phases: [
    { title: 'Scope', detail: 'one agent over the issue + whole diff: out-of-scope prune + over/under-delivery/leakage findings' },
    { title: 'Find', detail: 'per-file finders, one per applicable pillar lens (economy+convention merge into one quality finder in the pr profile; small diffs batch into shared calls)' },
    { title: 'Verify', detail: 'severity-tiered refutes (high-severity correctness test-grounded, the rest per-file batched) and a false-negative challenge (per-PR in the pr profile, per-file in backfill)' },
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
//   diffs,        { [absPath]: text } — per-file diff hunks. Presence makes finders diff-scoped
//                                       (and selects the pr profile); omit for whole-file review.
//   profile,      'pr'|'backfill'     — funnel shape; default: pr when diffs present, else backfill.
//   depth,        'gate'|'deep'       — review cost preset; default deep. gate selects spec-fidelity
//                                       + correctness, Sonnet verification, and no challenge.
//   finderShape,  'merged'|'specialist' — per-file finder composition; default: merged for pr,
//                                       specialist for backfill.
//   lenses,       [string]            — optional subset of pillar keys to run.
//   finderModel,  string              — model for Phase 0 + finders + batched/challenge verify (default 'sonnet').
//   verifyModel,  string              — model for the high-severity correctness refuter (default 'opus').
//   noChallenge,  bool                — skip the false-negative challenge (per-PR in pr, per-file in backfill).
//   noBuild,      bool                — the high-severity correctness refuter grounds read-only (no
//                                       scratch #[test] write + cargo test run); set true where no
//                                       toolchain/build cache is provisioned (default false).
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
const NO_BUILD = A.noBuild === true
const DIFFS = A.diffs || {}
const HAS_DIFFS = Object.keys(DIFFS).length > 0

// Profile parameterizes the funnel shape (issue #3015). The per-PR merge gate is gate depth over
// the pr profile: merged finders and small-diff batching are gated on the diff-line data that only
// diffs carry. Backfill is elective whole-file auditing and keeps specialist finders + per-file
// challenges pending A/B evidence. The mode-independent compressions (tiered/batched refutes, the
// rollup verdict fix) apply in both.
const PROFILE = A.profile || (HAS_DIFFS ? 'pr' : 'backfill')
if (PROFILE !== 'pr' && PROFILE !== 'backfill') throw new Error(`review: args.profile must be 'pr' or 'backfill', got ${JSON.stringify(A.profile)}`)
const DEPTH = A.depth || 'deep'
if (DEPTH !== 'gate' && DEPTH !== 'deep') throw new Error(`review: args.depth must be 'gate' or 'deep', got ${JSON.stringify(A.depth)}`)
const FINDER_SHAPE = A.finderShape || (PROFILE === 'pr' ? 'merged' : 'specialist')
if (FINDER_SHAPE !== 'merged' && FINDER_SHAPE !== 'specialist') throw new Error(`review: args.finderShape must be 'merged' or 'specialist', got ${JSON.stringify(A.finderShape)}`)
const VERIFY_MODEL = A.verifyModel || (DEPTH === 'gate' ? 'sonnet' : 'opus')
const NO_CHALLENGE = A.noChallenge === true || DEPTH === 'gate'

// Small-diff batching (pr profile): files whose diff changes fewer than SMALL_DIFF_LINES lines
// group into shared finder calls, at most BATCH_CAP files per call.
const SMALL_DIFF_LINES = 30
const BATCH_CAP = 5

// Pillars where a high-confidence finding STILL gets a verifier — a confident hallucination
// does the most damage in correctness, so it is never trusted on confidence alone.
const ALWAYS_VERIFY = new Set(['correctness'])
// Pillars worth a false-negative challenge (misses hide here). Economy/convention misses are
// low-stakes and the all-lens challenge cost a third of a run for zero yield, so it is restricted
// and runs on the cheap model.
const CHALLENGE_LENSES = new Set(['correctness', 'test-integrity'])

const base = (f) => f.split('/').slice(-1)[0]

// Changed-line count of a unified diff (added/removed rows, excluding the +++/--- file headers).
// The batching gate; the only file-size signal available to the sandbox, and only in the pr profile.
function changedLineCount(diff) {
  if (!diff) return Infinity
  let n = 0
  for (const row of diff.split('\n')) {
    if (row.startsWith('+++') || row.startsWith('---')) continue
    if (row[0] === '+' || row[0] === '-') n++
  }
  return n
}

const NORTH_STAR = `NORTH STAR (economy): "the fewest characters that still make sense." OVER-VERBOSE = a strictly shorter form reads at least as clearly and loses no meaning, safety, or exhaustiveness. OVER-TERSE = the code is compressed past clarity/safety and the fix ADDS characters to restore them. Ceremony (a long name, a confident abstraction, a clever one-liner) is not evidence — flag only when you can state the concrete better form AND why it is at least as clear/correct. When unsure, do not flag.`

const FILE_SPLIT_BAR = `FILE-SPLIT BAR (economy): large is a review smell, not a finding by itself. Raise category economy:file-split only when the file has a named responsibility seam and the proposed extraction lowers review/ownership burden without moving behavior. Strong candidates are files over roughly 1k lines with multiple cohorts (tool handlers + rendering helpers + tests; runtime handlers + DSP tests; parser + emitter + diagnostics) or diffs that add to an already-large coordination file. Do NOT flag a large file that is cohesive, generated-like, or a deliberate golden/scenario test unless the tangled responsibilities are concrete. Suggested form must name the child module/file(s) and what stays in the parent.`

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
- STRUCTURE: a god-module accreting unrelated responsibilities (name them); economy:file-split for a large file with a concrete module seam (apply FILE-SPLIT BAR); a helper far from its sole caller, a type upstream of its only consumer; long inline ::-paths a use import reads better. [dividers + suffix-siblings are convention's]
- VISIBILITY: pub broader than where it is referenced (-> narrow by privatizing the item or restructuring the module — scoped modifiers pub(crate)/pub(super) only where a crate's convention still uses them; in crates under the pub-or-private gate like aether-capabilities, resolve by privatization or module restructuring and NEVER recommend a scoped modifier — those are gate failures there); an invariant-bearing pub field (-> private + ctor, C-STRUCT-PRIVATE); an impl type leaked through a public signature. OVER-TERSE counter: a getter/setter pair over a field with NO invariant. [unreachable_pub/dead_code are Layer 0]
- CONTROL-FLOW: OVER-VERBOSE — a manual accumulate loop a map/filter/collect states clearer, rightward drift let-else flattens, a match on Option/Result that ?/map_or/ok_or shortens. OVER-TERSE — an if-let or _ => () dropping exhaustiveness a future variant should force; a combinator chain past readability; a one-liner hiding a side effect. [clippy's style/complexity rewrites are Layer 0]
- TYPE-DESIGN: primitive obsession (adjacent same-type ids; a bare f32 that is really Seconds vs Pixels -> newtype, C-NEWTYPE); a public/wire tuple or fixed array whose positions encode distinct axes, units, or ordering -> a named Schema type with named fields; a bool param that should be a 2-variant enum (C-CUSTOM-TYPE); a stringly-typed mode -> enum. OVER-TERSE counters: a newtype with no invariant or static distinction (pure ceremony), or replacing a genuinely index-addressed vector/matrix/color/buffer with field-name ceremony.`,
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
  : DEPTH === 'gate'
    ? ALL_LENSES.filter(l => l.key === 'spec-fidelity' || l.key === 'correctness')
    : ALL_LENSES
if (!SELECTED.length) throw new Error(`review: no lenses selected; valid keys are ${ALL_LENSES.map(l => l.key).join(', ')}`)

const SPEC_LENS = SELECTED.find(l => l.key === 'spec-fidelity')
const TEST_LENS = SELECTED.find(l => l.unit === 'test-file')

// The merged quality lens (finderShape 'merged'): one finder carries both advisory code pillars —
// economy and convention — so a changed file is read by two finders (correctness + quality) rather
// than three. It composes from the two source lens objects (concatenated taxonomies, both carve-outs,
// the economy north-star retained) and tags each finding with its pillar so the rollup rows and poster
// output stay byte-identical in shape. Correctness stays a solo specialist — it is the soft-hold pillar.
const ECONOMY_LENS = ALL_LENSES.find(l => l.key === 'economy')
const CONVENTION_LENS = ALL_LENSES.find(l => l.key === 'convention')
const QUALITY_LENS = {
  key: 'quality',
  name: 'Quality (economy + convention)',
  oracle: `${ECONOMY_LENS.oracle}; and ${CONVENTION_LENS.oracle}`,
  unit: 'file',
  gate: 'advisory',
  taxonomy: `Two pillars in one pass — tag every finding with pillar = economy or convention.

ECONOMY (${ECONOMY_LENS.oracle}):
${ECONOMY_LENS.taxonomy}

CONVENTION & ARCHITECTURE (${CONVENTION_LENS.oracle}):
${CONVENTION_LENS.taxonomy}`,
  carveOut: `${ECONOMY_LENS.carveOut}

CONVENTION carve-out: ${CONVENTION_LENS.carveOut}`,
}

// Per-file lenses, honoring the finder shape. 'merged' collapses the economy + convention members
// into the single quality lens; 'specialist' keeps all three separate.
let FILE_LENSES = SELECTED.filter(l => l.unit === 'file')
if (FINDER_SHAPE === 'merged') {
  const merged = []
  let addedQuality = false
  for (const l of FILE_LENSES) {
    if (l.key === 'economy' || l.key === 'convention') {
      if (!addedQuality) { merged.push(QUALITY_LENS); addedQuality = true }
    } else merged.push(l)
  }
  FILE_LENSES = merged
}

// Lens subset for out-of-scope (drive-by / unrelated) changed files. Phase 0's spec filter keeps
// these files in the funnel with correctness ALONE — a bug does not become acceptable because the
// file was out of scope, and correctness is the one soft-hold pillar. The advisory lenses (economy,
// convention, test-integrity) stay pruned: nitpicking the style of a to-be-reverted edit is noise.
// Derived from FILE_LENSES (already intersected with SELECTED), so a lens-restricted run that
// excludes correctness leaves this empty and prunes out-of-scope files entirely, as before.
const OUT_OF_SCOPE_LENSES = FILE_LENSES.filter(l => l.key === 'correctness')

// Deterministic scope resolution (no agents): one entry per file, carrying the lenses that
// apply to it (code lenses for files, test-integrity for testFiles) and its diff hunk if any.
// In the pr profile, small-diff code-only entries are grouped into shared finder calls.
function resolveScope() {
  const all = [...new Set([...FILES, ...TEST_FILES])]
  const entries = all.map(f => {
    const lenses = []
    if (FILES.includes(f)) lenses.push(...FILE_LENSES)
    if (TEST_FILES.includes(f) && TEST_LENS) lenses.push(TEST_LENS)
    return { file: f, lenses, diff: DIFFS[f] || null }
  }).filter(e => e.lenses.length)
  return PROFILE === 'pr' ? batchSmallDiffs(entries) : entries
}

// Group consecutive small-diff, code-only entries (no test lens — a batched call runs FILE_LENSES)
// into batch entries of up to BATCH_CAP files. A lone small file stays a single entry. The pr
// profile only; a batch entry is { batch, members: [{file, diff}], lenses }.
function batchSmallDiffs(entries) {
  const out = []
  let bucket = []
  const flush = () => {
    if (!bucket.length) return
    if (bucket.length === 1) out.push(bucket[0])
    else out.push({ batch: true, members: bucket.map(e => ({ file: e.file, diff: e.diff })), lenses: FILE_LENSES })
    bucket = []
  }
  for (const e of entries) {
    const codeOnly = e.lenses.every(l => l.unit === 'file')
    const small = e.diff && changedLineCount(e.diff) < SMALL_DIFF_LINES
    if (codeOnly && small) {
      bucket.push(e)
      if (bucket.length >= BATCH_CAP) flush()
    } else {
      flush()
      out.push(e)
    }
  }
  flush()
  return out
}

// Flatten scope entries (single or batch) into { file, diff } units — for the whole-change agents
// (spec filter, per-PR challenge) that judge the diff set rather than fanning out per entry.
function scopeUnits(entries) {
  const units = []
  for (const e of entries) {
    if (e.batch) for (const m of e.members) units.push({ file: m.file, diff: m.diff })
    else units.push({ file: e.file, diff: e.diff })
  }
  return units
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
    confidence: { type: 'string', enum: ['high', 'medium', 'low'], description: 'low/medium routes to a refuter; high goes straight to the rollup (except correctness, always verified)' },
    recommendation: { type: 'string', enum: ['fix', 'remove', 'rewrite', 'promote-lint'] },
    current_form: { type: 'string', description: 'the current code / name / test, briefly' },
    suggested_form: { type: 'string', description: 'the proposed action — code for economy/convention; the failing input + correct behavior for correctness; "remove" or the rewrite for test-integrity' },
    rationale: { type: 'string', description: 'the judgment: why the fix is at least as clear/correct/safe — not a restatement of the code' },
    direction: { type: 'string', enum: ['over-verbose', 'over-terse'], description: 'ECONOMY findings only' },
    char_delta: { type: 'integer', description: 'ECONOMY findings only: suggested length minus current length, in characters (negative = shorter)' },
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

// The finder schema for a lens/entry: the merged quality lens requires a `pillar` on every finding
// (so rollup rows keep economy/convention fidelity), and a batched call requires a `file` on every
// finding and lint candidate (so rows attribute to the right member file). One built from FIND_SCHEMA
// so the taxonomies, row shape, and lint feed stay single-sourced.
function findSchema({ pillar, batched }) {
  const props = { ...FINDING_ITEM.properties }
  const required = [...FINDING_ITEM.required]
  if (pillar) {
    props.pillar = { type: 'string', enum: ['economy', 'convention'], description: 'MERGED quality lens: which pillar this finding belongs to' }
    required.push('pillar')
  }
  if (batched) {
    props.file = { type: 'string', description: 'absolute path of the batched file this finding is in' }
    required.push('file')
  }
  const findingItem = { ...FINDING_ITEM, required, properties: props }
  const lintItem = batched
    ? {
      type: 'object',
      additionalProperties: false,
      required: ['file', 'symbol', 'note'],
      properties: { file: { type: 'string', description: 'absolute path of the batched file' }, ...FIND_SCHEMA.properties.lintCandidates.items.properties },
    }
    : FIND_SCHEMA.properties.lintCandidates.items
  return {
    ...FIND_SCHEMA,
    properties: {
      ...FIND_SCHEMA.properties,
      findings: { type: 'array', items: findingItem },
      lintCandidates: { ...FIND_SCHEMA.properties.lintCandidates, items: lintItem },
    },
  }
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

// Batch refuter verdict — one adjudication per submitted finding, keyed by its index in the list.
const BATCH_VERDICT = {
  type: 'object',
  additionalProperties: false,
  required: ['verdicts'],
  properties: {
    verdicts: {
      type: 'array',
      description: 'one verdict per submitted finding, keyed by its index (#0, #1, …)',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['index', 'final_verdict', 'rationale'],
        properties: {
          index: { type: 'integer', description: 'the finding index from the submitted list' },
          final_verdict: { type: 'string', enum: ['confirmed', 'false-positive', 'uncertain'] },
          rationale: { type: 'string', description: 'why it holds under the strict bar, or why it is a false positive' },
        },
      },
    },
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

// Per-PR challenge (pr profile): one agent over the whole change, so each missed finding must carry
// the file it is in and the lens the finders missed it under before the tiered refute path can route it.
const PR_CHALLENGE_MISSED_ITEM = {
  ...FINDING_ITEM,
  required: [...FINDING_ITEM.required, 'file', 'lens'],
  properties: {
    ...FINDING_ITEM.properties,
    file: { type: 'string', description: 'absolute path of the file the missed finding is in' },
    lens: { type: 'string', enum: ['correctness', 'test-integrity'], description: 'the pillar the finder passes missed it under' },
  },
}

const PR_CHALLENGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['final_verdict', 'missed'],
  properties: {
    final_verdict: { type: 'string', enum: ['clean-confirmed', 'missed', 'uncertain'] },
    missed: { type: 'array', description: 'findings the per-file finders overlooked across the whole change (empty unless final_verdict = missed)', items: PR_CHALLENGE_MISSED_ITEM },
  },
}

function specPrompt(issue, entries) {
  const units = scopeUnits(entries)
  const fileList = units.map(u => u.diff ? `--- ${u.file}\n${u.diff}` : u.file).join('\n\n')
  const diffed = units.some(u => u.diff)
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
  const units = f.batch ? f.members : [{ file: f.file, diff: f.diff }]
  const multi = units.length > 1
  const blocks = units.map(u => u.diff
    ? `FILE: ${u.file}\nFocus on the CHANGED lines below; open the full file for context, and scope every finding to the change.\nDIFF:\n${u.diff}`
    : `FILE: ${u.file}\nRead the full file (and the types/helpers it references where you need them to judge).`).join('\n\n')
  const econ = lens.key === 'economy' || lens.key === 'quality'
  const north = econ ? `\n${NORTH_STAR}\n\n${FILE_SPLIT_BAR}\n` : ''
  const pillarNote = lens.key === 'quality' ? ' This lens spans two pillars — tag every finding with pillar = economy or convention.' : ''
  const fileNote = f.batch ? ' Report which file each finding (and lint candidate) belongs to in its file field.' : ''
  return `You are one specialist judge on a code-review panel, auditing ${multi ? 'a batch of Rust files' : 'a Rust file'} under a single lens.

Lens: ${lens.name}
Oracle (what you judge against): ${lens.oracle}
${lens.taxonomy}

CARVE-OUT (do NOT raise as judgment findings — route to lintCandidates or the named pillar): ${lens.carveOut}
${north}
${blocks}

Report every site this lens flags as a finding: symbol + approximate line, category (the sub-shape), severity, the current form, the suggested fix, a rationale stating the judgment (why the fix is at least as clear/correct/safe — not a restatement), and your confidence.${pillarNote}${econ ? ' Fill direction (over-verbose|over-terse) and char_delta on every economy finding.' : ''}${fileNote} Put mechanically-decidable observations in lintCandidates, not findings. Report nothing this lens does not own — another panelist covers the others. Report ONLY sites located in the file(s) above: if you notice an issue in a different file they reference, do not report it (that file gets its own finder). If the file is clean under this lens, return an empty findings array. Be precise and conservative: a confident story is not a finding; flag only when you can name the concrete better form AND why it wins.`
}

function refutePrompt(file, fd, lens) {
  const corr = lens.key === 'correctness'
  const step2 = NO_BUILD
    ? `2. No test covers the claim and no build is available here — ground by reading the existing #[test]s and the code path; if a high-severity claim has no covering test, state that in the rationale and return final_verdict='uncertain' rather than confirming on inspection alone.`
    : `2. If no test covers the claim and the finding is high-severity, WRITE one and RUN it: add a focused #[test], run \`cargo test -p <crate> <name>\` from the crate root, let the result decide, then delete the scratch test.`
  const grounding = corr
    ? `\nGROUND THE VERDICT IN TESTS, not inspection — a confident reading of subtle code (math conventions, sign order, edge cases) is exactly where this lens hallucinates a bug.
1. Read the existing #[test]s for this item. A passing test that pins the claimed-broken behavior REFUTES the finding (final_verdict='false-positive') — cite the test by name.
${step2}
3. NEVER uphold a finding whose suggested fix would break a currently-passing test — check the fix against the suite before confirming.\n`
    : ''
  return `A code-review finding was raised under the ${lens.name} lens. Decide whether it survives a STRICT bar — do not rescue it with a plausible story, and do not reject a real issue out of conservatism.

File: ${file}
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

function batchRefutePrompt(file, items) {
  const list = items.map((it, i) => `#${i} [${it.lens.name}] ${it.fd.symbol} (around line ${it.fd.line})
  category: ${it.fd.category}
  current form: ${it.fd.current_form}
  suggested fix: ${it.fd.suggested_form}
  finder rationale: ${it.fd.rationale}`).join('\n\n')
  return `A batch of code-review findings on one file was raised under various lenses. Adjudicate EACH against a STRICT bar — do not rescue one with a plausible story, and do not reject a real issue out of conservatism. This is a READ-ONLY pass: read the site and the code it depends on; you cannot run tests.

File: ${file}

FINDINGS (adjudicate every one, keyed by its index):
${list}

${BAR}

For a CORRECTNESS finding, ground the verdict in tests you can READ — a passing #[test] that pins the claimed-broken behavior refutes it; since you cannot run code, mark a subtle correctness claim you cannot settle by reading as 'uncertain' rather than confirming it.

Return one verdict per finding, keyed by its index: final_verdict='confirmed' when it genuinely meets the bar, 'false-positive' when the code is fine as written (load-bearing verbosity, correct behavior, a passing test already pins it, the rule does not apply), or 'uncertain' when you cannot read the relevant code.`
}

function challengePrompt(file, lens) {
  return `The ${lens.name} lens reported NO findings for this file. CHALLENGE that clean verdict — re-read the file and look specifically for what this lens catches that a first pass missed.

File: ${file}
Oracle: ${lens.oracle}
${lens.taxonomy}

CARVE-OUT (do NOT raise — mechanically settled or another pillar's): ${lens.carveOut}

${BAR}

If the file is genuinely clean under this lens, final_verdict='clean-confirmed' and missed=[]. If you find real issues that meet the bar, final_verdict='missed' and list them in missed[] using the full finding shape (symbol, line, category, severity, confidence, recommendation, current_form, suggested_form, rationale). Use 'uncertain' only when you cannot read the relevant code.`
}

function prChallengePrompt(entries, lenses) {
  const units = scopeUnits(entries).filter(u => u.diff)
  const blocks = units.map(u => `FILE: ${u.file}\nDIFF:\n${u.diff}`).join('\n\n')
  const taxo = lenses.map(l => `### ${l.name}\nOracle: ${l.oracle}\n${l.taxonomy}\n\nCARVE-OUT: ${l.carveOut}`).join('\n\n')
  return `The per-file finder passes reported their findings for this change. CHALLENGE the clean verdict ACROSS THE WHOLE CHANGE under the recall-sensitive pillars below — re-read every diff hunk (open the full files where you need context) and look specifically for what these lenses catch that the finders missed.

CHANGED HUNKS:
${blocks}

PILLARS TO CHALLENGE:
${taxo}

${BAR}

If the change is genuinely clean under these lenses, final_verdict='clean-confirmed' and missed=[]. If you find real issues that meet the strict bar, final_verdict='missed' and list each in missed[] using the full finding shape, tagging every item with its file and the lens (correctness|test-integrity) the finders missed it under. Use 'uncertain' only when you cannot read the relevant code.`
}

// The verify funnel, shared by the per-file stage and the per-PR challenge (issue #3015 item 3).
// Each item is { fd, lens, file }; the verdict is attached back onto item.fd.verify. High-severity
// correctness gets a per-finding, test-grounded refuter on the Opus verify model (the soft-hold
// pillar, where a confident hallucination costs most); every other refute-bound finding goes to one
// read-only batch adjudicator per file on the cheap finder model, verdicts keyed by list index.
async function refuteFlags(items) {
  const isTopTier = (it) => it.lens.key === 'correctness' && it.fd.severity === 'high'
  const perFinding = items.filter(isTopTier)
  const batched = items.filter(it => !isTopTier(it))

  const single = parallel(perFinding.map(it => () =>
    agent(refutePrompt(it.file, it.fd, it.lens), {
      label: `refute:${it.lens.key}:${it.fd.symbol}`.slice(0, 80),
      phase: 'Verify',
      model: VERIFY_MODEL,
      // A Bash-capable agent so it can write/run a grounding test, not just read.
      agentType: 'general-purpose',
      schema: VERDICT,
    }).then(v => { it.fd.verify = v })
  ))

  const byFile = new Map()
  for (const it of batched) { const a = byFile.get(it.file) || []; a.push(it); byFile.set(it.file, a) }
  const batches = parallel([...byFile.entries()].map(([file, its]) => () =>
    agent(batchRefutePrompt(file, its), {
      label: `refute-batch:${base(file)}`.slice(0, 80),
      phase: 'Verify',
      model: FINDER_MODEL,
      schema: BATCH_VERDICT,
    }).then(res => {
      for (const v of ((res && res.verdicts) || [])) {
        const it = its[v.index]
        if (it) it.fd.verify = { final_verdict: v.final_verdict, rationale: v.rationale }
      }
    })
  ))

  await single
  await batches
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
  if (out.size) {
    // Keep out-of-scope files in the funnel with the reduced correctness-only lens set (a demoted
    // single entry) rather than dropping them, so a drive-by file's bugs are still caught and can
    // soft-hold. Out-of-scope members of a small-diff batch are lifted out into their own demoted
    // entries so the batch's uniform FILE_LENSES stays honored. When correctness is not selected,
    // OUT_OF_SCOPE_LENSES is empty and the file is pruned entirely, as before.
    inScope = scoped.flatMap(e => {
      if (!e.batch) {
        if (!out.has(e.file)) return [e]
        return OUT_OF_SCOPE_LENSES.length ? [{ ...e, lenses: OUT_OF_SCOPE_LENSES }] : []
      }
      const kept = e.members.filter(m => !out.has(m.file))
      const demoted = OUT_OF_SCOPE_LENSES.length
        ? e.members.filter(m => out.has(m.file)).map(m => ({ file: m.file, lenses: OUT_OF_SCOPE_LENSES, diff: m.diff }))
        : []
      const batch = kept.length ? [{ ...e, members: kept }] : []
      return [...batch, ...demoted]
    })
    const note = OUT_OF_SCOPE_LENSES.length ? 'reduced to the correctness lens (advisory lenses pruned)' : 'pruned from the per-file passes'
    log(`spec-fidelity: ${out.size} out-of-scope file(s) ${note}`)
  }
}

// Phases 1+2 — per-file funnel, pipelined (no barrier between entries). Stage 1 fans out the
// applicable finders (one per lens; a batch entry runs each lens over all its member files);
// stage 2 runs the tiered/batched refutes, plus the per-file clean-lens challenge in backfill
// (the pr profile runs one per-PR challenge post-pipeline instead).
const results = await pipeline(
  inScope,
  (f) => parallel(f.lenses.map(lens => () =>
    agent(findPrompt(f, lens), {
      label: f.batch ? `find:${lens.key}:batch${f.members.length}` : `find:${lens.key}:${base(f.file)}`,
      phase: 'Find',
      model: FINDER_MODEL,
      schema: findSchema({ pillar: lens.key === 'quality', batched: !!f.batch }),
    }).then(r => ({ lens, r }))
  )),
  async (lensRuns, f) => {
    const runs = (lensRuns || []).filter(Boolean).filter(x => x.r)
    const files = f.batch ? f.members.map(m => m.file) : [f.file]
    if (!runs.length) return { files, runs: [], challenged: [] }

    const flags = []
    for (const x of runs) for (const fd of (x.r.findings || [])) flags.push({ fd, lens: x.lens, file: fd.file || x.r.file || files[0] })
    // Correctness is verified regardless of confidence; other lenses skip high-confidence flags.
    const toRefute = flags.filter(fl => ALWAYS_VERIFY.has(fl.lens.key) || fl.fd.confidence !== 'high')
    const refute = refuteFlags(toRefute)

    // Per-file clean-lens challenge — backfill only, on the cheap model.
    const cleanRuns = (NO_CHALLENGE || PROFILE === 'pr') ? [] : runs.filter(x => !(x.r.findings || []).length && CHALLENGE_LENSES.has(x.lens.key))
    const challenge = parallel(cleanRuns.map(x => () =>
      agent(challengePrompt(x.r.file || files[0], x.lens), { label: `challenge:${x.lens.key}:${base(x.r.file || files[0])}`, phase: 'Verify', model: FINDER_MODEL, schema: CHALLENGE })
        .then(c => ({ lens: x.lens, file: x.r.file || files[0], challenge: c }))
    ))

    await refute
    return { files, runs, challenged: (await challenge).filter(Boolean) }
  }
)

const clean = results.filter(Boolean)

// Per-PR challenge (pr profile) — one agent over all in-scope diff hunks looks for what the finders
// missed under the recall-sensitive pillars, then its missed findings run the tiered refute path
// before any can reach confirmed. This closes the one path where a challenger-missed finding used to
// reach confirmed unverified. noChallenge skips it.
let prChallengeMissed = []
const challengeLenses = SELECTED.filter(l => CHALLENGE_LENSES.has(l.key))
if (PROFILE === 'pr' && !NO_CHALLENGE && challengeLenses.length && scopeUnits(inScope).some(u => u.diff)) {
  phase('Verify')
  const ch = await agent(prChallengePrompt(inScope, challengeLenses), { label: 'challenge:pr', phase: 'Verify', model: FINDER_MODEL, schema: PR_CHALLENGE_SCHEMA })
  if (ch && ch.final_verdict === 'missed') {
    const lensByKey = new Map(challengeLenses.map(l => [l.key, l]))
    for (const m of (ch.missed || [])) {
      const lens = lensByKey.get(m.lens) || challengeLenses[0]
      prChallengeMissed.push({ fd: m, lens, file: m.file })
    }
    if (prChallengeMissed.length) await refuteFlags(prChallengeMissed)
  }
}

// Deterministic rollup. A finding is CONFIRMED three ways: high-confidence (no live refuter, or an
// ALWAYS_VERIFY finding its refuter upheld), refuter 'confirmed' (a low/med flag upheld), or
// challenger 'missed' (a clean lens overlooked it — refute-gated in the pr profile). 'spared' = a
// flag the refuter overturned, now including a refuted high-confidence correctness finding (issue
// #3015 item 2 — the Opus refuter's verdict is consulted rather than discarded). gate = soft-hold
// only for a high-severity finding on a soft-hold pillar (spec/correctness); everything else advisory.
// Spec-fidelity findings never flow through rowOf/gateFor — they live in the separate Phase 0
// `spec` channel — so a high-severity spec finding is routed into softHolds directly, below.
const gateFor = (lens, severity) => (lens.gate === 'soft-hold' && severity === 'high') ? 'soft-hold' : 'advisory'
const rowOf = (file, lens, fd, source) => ({
  file, pillar: fd.pillar || lens.key, source, category: fd.category, line: fd.line, symbol: fd.symbol,
  severity: fd.severity, gate: gateFor(lens, fd.severity), recommendation: fd.recommendation,
  suggested_form: fd.suggested_form, direction: fd.direction, char_delta: fd.char_delta,
})

const totals = { files: 0, finders: 0, findings: 0, confirmed: 0, falsePositives: 0, challengerMissed: 0, softHolds: 0 }
const confirmed = [], spared = [], uncertain = [], lintCandidates = []

for (const e of clean) {
  if (!e.runs || !e.runs.length) continue

  for (const x of e.runs) {
    totals.finders++
    const runFile = x.r.file || (e.files && e.files[0])
    for (const lc of (x.r.lintCandidates || [])) lintCandidates.push({ file: base(lc.file || runFile), lens: x.lens.key, symbol: lc.symbol, note: lc.note })
    for (const fd of (x.r.findings || [])) {
      totals.findings++
      const lens = x.lens
      // Full path, not a basename — the dedup key and grouped map below key on
      // this, and a basename collides across same-named files (mod.rs, kinds.rs).
      const file = fd.file || runFile
      const v = fd.verify
      if (ALWAYS_VERIFY.has(lens.key) && fd.confidence === 'high') {
        // ALWAYS_VERIFY consults the refuter even at high confidence: a refuted high-confidence
        // correctness finding is spared, an upheld one confirmed with source high-confidence.
        if (v && v.final_verdict === 'false-positive') { spared.push({ ...rowOf(file, lens, fd, 'spared'), reason: v.rationale }); totals.falsePositives++ }
        else confirmed.push(rowOf(file, lens, fd, 'high-confidence'))
      } else if (fd.confidence === 'high') {
        confirmed.push(rowOf(file, lens, fd, 'high-confidence'))
      } else if (v && v.final_verdict === 'confirmed') {
        confirmed.push(rowOf(file, lens, fd, 'refuter'))
      } else if (v && v.final_verdict === 'false-positive') {
        spared.push({ ...rowOf(file, lens, fd, 'spared'), reason: v.rationale }); totals.falsePositives++
      } else {
        uncertain.push({ ...rowOf(file, lens, fd, 'uncertain'), stage: 'refute', note: (v && v.rationale) || 'no verify result' })
      }
    }
  }

  // Backfill per-file clean-lens challenge — a 'missed' finding goes straight to confirmed (the pr
  // profile instead routes its per-PR challenge misses through the refuter, below).
  for (const ch of (e.challenged || [])) {
    const v = ch.challenge
    const file = ch.file || (e.files && e.files[0])
    if (v && v.final_verdict === 'missed') {
      for (const m of (v.missed || [])) { totals.challengerMissed++; confirmed.push(rowOf(file, ch.lens, m, 'challenger-missed')) }
    } else if (v && v.final_verdict === 'uncertain') {
      uncertain.push({ file, pillar: ch.lens.key, symbol: '(clean-lens challenge)', stage: 'challenge', note: 'challenger could not read the relevant code' })
    }
  }
}

// Per-PR challenge misses (pr profile) — refute-gated before confirmed, unlike the backfill path.
for (const it of prChallengeMissed) {
  const { fd, lens } = it
  const file = it.file
  const v = fd.verify
  if (v && v.final_verdict === 'confirmed') { totals.challengerMissed++; confirmed.push(rowOf(file, lens, fd, 'challenger-missed')) }
  else if (v && v.final_verdict === 'false-positive') { spared.push({ ...rowOf(file, lens, fd, 'spared'), reason: v.rationale }); totals.falsePositives++ }
  else uncertain.push({ ...rowOf(file, lens, fd, 'uncertain'), stage: 'challenge', note: (v && v.rationale) || 'no verify result' })
}

// Dedup backstop: a finder that strayed into another file, two sub-lenses overlapping, or a per-PR
// challenge miss on a site a finder already raised can produce the same row twice. Keep the first
// row per (file, pillar, category, line).
const seenRows = new Set()
const deduped = []
for (const r of confirmed) {
  const k = `${r.file}:${r.pillar}:${r.category}:${r.line}`
  if (seenRows.has(k)) continue
  seenRows.add(k); deduped.push(r)
}

const processedFiles = new Set()
for (const e of clean) if (e.runs && e.runs.length) for (const fl of (e.files || [])) processedFiles.add(fl)
totals.files = processedFiles.size
totals.confirmed = deduped.length
// A high-severity spec finding never enters confirmed/deduped (it would double-render alongside
// rollup.spec.findings) but still counts as a hold — the poster already gives it label teeth.
const specSoftHolds = ((spec && spec.findings) || []).filter(f => f.severity === 'high').map(f => ({
  file: base(f.file), pillar: 'spec-fidelity', source: 'spec', category: f.category, line: null,
  symbol: f.symbol, severity: f.severity, gate: 'soft-hold', recommendation: undefined,
  suggested_form: f.description, direction: undefined, char_delta: undefined,
}))
const softHolds = [...deduped.filter(r => r.gate === 'soft-hold'), ...specSoftHolds]
totals.softHolds = softHolds.length

// grouped: by file then pillar, so the rollup drops straight into /sketch.
const grouped = {}
for (const r of deduped) ((grouped[r.file] ||= {})[r.pillar] ||= []).push(r)
const bySource = deduped.reduce((a, r) => ((a[r.source] = (a[r.source] || 0) + 1), a), {})
const specCount = spec ? (spec.findings || []).length : 0

log(`review [${PROFILE}/${FINDER_SHAPE}]: ${totals.files} files, ${totals.finders} finders -> ${totals.confirmed} confirmed (high-conf ${bySource['high-confidence'] || 0}, refuter ${bySource['refuter'] || 0}, challenger ${bySource['challenger-missed'] || 0}), ${softHolds.length} SOFT-HOLD, ${spared.length} spared, ${uncertain.length} uncertain, ${lintCandidates.length} lint candidates, ${specCount} spec findings`)

return {
  rollup: {
    totals,
    spec: spec ? { findings: spec.findings || [], outOfScope: spec.outOfScope || [] } : null,
    softHolds, confirmed: deduped, grouped, lintCandidates, spared, uncertain,
  },
  files: clean,
}
