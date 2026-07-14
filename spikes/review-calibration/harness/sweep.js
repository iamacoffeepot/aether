export const meta = {
  name: 'review-calibration-sweep',
  description: 'Matrix sweep (model x effort) of the review finder over seeded-bug + clean-control items, graded against an answer key',
  phases: [
    { title: 'Find', detail: 'one production-faithful finder per cell x item (x trial on frontier cells)' },
  ],
}

// args = { items: [{id, file, lens ('correctness'|'test-integrity'), level, shape, fn, line, diff}],
//          cells: [{model, effort, trials}] }
const ITEMS = args.items
const CELLS = args.cells

// Verbatim from .claude/workflows/review.js — the two calibrated lenses.
const CORRECTNESS = {
  key: 'correctness',
  name: 'Correctness',
  oracle: "the code's own contract — what it implies it should do",
  taxonomy: `Named bug-shapes (NOT "find any bug" — flag only these, each with a concrete misbehaving input/path).
- SWALLOWED ERROR: a fallible result dropped — let _ = on a Result, .ok() discarding an Err, unwrap()/expect() on a runtime-fallible path, an omitted ? that loses an error, an error remapped to a less-informative one.
- MISSING BOUNDS CAP: recursion or geometrically-/user-derived iteration without the CLAUDE.md-mandated depth/budget cap that returns an error rather than overflowing; unbounded growth; integer overflow on user-derived arithmetic.
- SILENT INCOMPLETENESS: TODO / todo!() / unimplemented!() / a "for now" stub / a branch or match arm that no-ops where the logic is required.
- INVARIANT VIOLATION: new code that can put a type into a state its invariant forbids (flag the violation; the over-broad pub exposing the field is economy/visibility).
- RESOURCE LEAK: a handle / subscription / mixer-lane / texture / spawned child acquired without the matching release on every path, including early-return and error paths.
- CONCURRENCY: a data race / lost update / lock-order hazard. SCOPED: actor state is single-threaded behind its run-token (ADR-0038), so this shape is N/A in actor/component code — apply it ONLY in aether-substrate / native chassis code.`,
  carveOut: `Judgment about behavior, not lints. rustc owns type/borrow safety (on Rust source only); where a clippy lint already fires, route it to lintCandidates. Do not flag style/verbosity (economy) or wrong-feature (spec). A finding must name the concrete input or path that misbehaves.`,
}
const TEST_INTEGRITY = {
  key: 'test-integrity',
  name: 'Test integrity',
  oracle: 'the testing policy (docs/guide/testing.md) — what owned logic the test exercises',
  taxonomy: `THE DECISIVE QUESTION: what logic owned by THIS crate does the test exercise? If the honest answer is "none — it restates a declaration or re-runs machinery another crate owns", it is JUNK however much ceremony surrounds it. Junk shapes: mirror (incl. derived-constant: assert_eq!(K::NAME, "literal")), derive-only-roundtrip (symmetric decode(encode(x))==x over plain #[derive]s), not-owned (std/serde/wgpu/the Kind/Schema/Config derives), re-tests-machinery (descriptors::all() membership, SchemaType-shape asserts, config resolution, id/lineage hashing), mock-theater, no-assertion/echo, vacuous, bulk-dup, coverage-chasing.
TRIPWIRE (the only flat-value keep): the pinned value is COMPUTED — a hash, golden bytes, a derived KindId number — so it drifts when the producing LOGIC changes. A name pinned against its own #[kind(name)] literal or a SchemaType keyword restatement is a mirror, not a tripwire, even with a // Tripwire: comment.`,
  carveOut: `Only #[test]/#[tokio::test] fns. The non-test code the test drives is correctness/economy's. Read the full policy at docs/guide/testing.md — this taxonomy is its summary. Express a junk test as a finding: recommendation 'remove' (or 'rewrite'), category = the junk shape, current_form = the test signature.`,
}
const LENSES = { correctness: CORRECTNESS, 'test-integrity': TEST_INTEGRITY }

const FINDING_ITEM = {
  type: 'object',
  additionalProperties: false,
  required: ['symbol', 'line', 'category', 'severity', 'confidence', 'recommendation', 'current_form', 'suggested_form', 'rationale'],
  properties: {
    symbol: { type: 'string', description: 'the item — fn/struct/field/binding/test name + a short locator' },
    line: { type: 'integer', description: 'approximate line of the site (advisory)' },
    category: { type: 'string', description: 'the lens sub-shape' },
    severity: { type: 'string', enum: ['high', 'medium', 'low'] },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
    recommendation: { type: 'string', enum: ['fix', 'remove', 'rewrite', 'promote-lint'] },
    current_form: { type: 'string' },
    suggested_form: { type: 'string' },
    rationale: { type: 'string' },
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
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['symbol', 'note'],
        properties: { symbol: { type: 'string' }, note: { type: 'string' } },
      },
    },
  },
}

// Verbatim findPrompt shape from review.js (single-file, diff-scoped).
function findPrompt(item, lens) {
  const block = `FILE: ${item.file}\nFocus on the CHANGED lines below; open the full file for context, and scope every finding to the change.\nDIFF:\n${item.diff}`
  return `You are one specialist judge on a code-review panel, auditing a Rust file under a single lens.

Lens: ${lens.name}
Oracle (what you judge against): ${lens.oracle}
${lens.taxonomy}

CARVE-OUT (do NOT raise as judgment findings — route to lintCandidates or the named pillar): ${lens.carveOut}

${block}

Report every site this lens flags as a finding: symbol + approximate line, category (the sub-shape), severity, the current form, the suggested fix, a rationale stating the judgment (why the fix is at least as clear/correct/safe — not a restatement), and your confidence. Put mechanically-decidable observations in lintCandidates, not findings. Report nothing this lens does not own — another panelist covers the others. Report ONLY sites located in the file(s) above: if you notice an issue in a different file they reference, do not report it (that file gets its own finder). If the file is clean under this lens, return an empty findings array. Be precise and conservative: a confident story is not a finding; flag only when you can name the concrete better form AND why it wins.`
}

const norm = (s) => (s || '').toLowerCase()
function grade(item, result) {
  const findings = (result && result.findings) || []
  const fnName = norm(item.fn)
  const isHit = (f) => {
    if (fnName && (norm(f.symbol).includes(fnName) || norm(f.current_form).includes(fnName))) return true
    return item.line > 0 && typeof f.line === 'number' && Math.abs(f.line - item.line) <= 10
  }
  const hits = item.level === 'CLEAN' ? [] : findings.filter(isHit)
  const fps = findings.filter(f => !hits.includes(f))
  return {
    hit: hits.length > 0,
    hitSeverity: hits.length ? hits[0].severity : null,
    hitConfidence: hits.length ? hits[0].confidence : null,
    fpCount: fps.length,
    findings: findings.map(f => ({ symbol: f.symbol, line: f.line, category: f.category, severity: f.severity, confidence: f.confidence, matched: hits.includes(f) })),
    agentFailed: result === null,
  }
}

phase('Find')
const jobs = []
for (const cell of CELLS) {
  for (let trial = 0; trial < (cell.trials || 1); trial++) {
    for (const item of ITEMS) {
      jobs.push({ cell, trial, item })
    }
  }
}
log(`sweep: ${CELLS.length} cells, ${ITEMS.length} items, ${jobs.length} finder calls`)

const results = await parallel(jobs.map(j => () =>
  agent(findPrompt(j.item, LENSES[j.item.lens]), {
    label: `find:${j.cell.model}-${j.cell.effort}${j.trial ? `#${j.trial + 1}` : ''}:${j.item.id}`,
    phase: 'Find',
    model: j.cell.model,
    effort: j.cell.effort,
    schema: FIND_SCHEMA,
  }).then(r => ({ ...j, graded: grade(j.item, r) }))
    .catch(e => ({ ...j, graded: grade(j.item, null), error: String(e) }))
))

const rows = results.filter(Boolean).map(r => ({
  cell: `${r.cell.model}:${r.cell.effort}`,
  trial: r.trial,
  item: r.item.id,
  level: r.item.level,
  lens: r.item.lens,
  ...r.graded,
}))

// Per-cell aggregate.
const cells = {}
for (const r of rows) {
  const c = (cells[r.cell] ||= { calls: 0, failed: 0, byLevel: {}, fp: 0, cleanFp: 0, cleanItems: 0 })
  c.calls++
  if (r.agentFailed) { c.failed++; continue }
  if (r.level === 'CLEAN') { c.cleanItems++; c.cleanFp += r.fpCount }
  else {
    const b = (c.byLevel[r.level] ||= { n: 0, hits: 0 })
    b.n++; if (r.hit) b.hits++
  }
  c.fp += r.fpCount
}

return { cells, rows }
