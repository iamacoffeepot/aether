export const meta = {
  name: 'review-refuter-calibration',
  description: 'Matrix sweep (model x effort) of the review refuter over real vs fabricated findings',
  phases: [
    { title: 'Refute', detail: 'one production-faithful read-only refuter per cell x finding' },
  ],
}

const ITEMS = REPLACE_ITEMS
const CELLS = [
  { model: 'sonnet', effort: 'medium' },
  { model: 'sonnet', effort: 'high' },
  { model: 'opus', effort: 'medium' },
  { model: 'opus', effort: 'high' },
  { model: 'fable', effort: 'medium' },
]

const BAR = `The bar for keeping a finding (strict, all lenses): the fix must be strictly better, not merely different.
- ECONOMY: the suggested form is "the fewest characters that still make sense" — shorter AND at least as clear/safe (over-verbose), or the terse form genuinely costs clarity/safety so the longer fix is worth it (over-terse). Reject taste ("I would write it differently").
- CORRECTNESS: name the concrete input or code path that misbehaves. Reject "could be unsafe" with no path; reject a hazard rustc/borrowck already prevents.
- CONVENTION: cite the CLAUDE.md / ADR rule it breaks. Reject anything Layer 0 (clippy -D warnings, fmt, Qodana, check-no-dividers, the disallowed-methods bans) already gates — that is a lintCandidate, not a judgment finding.
- TEST-INTEGRITY: junk unless the test exercises owned logic a plausible edit to THIS crate would break (and that the shared derive/codec/registry machinery's own tests would NOT already catch), OR it pins a COMPUTED value (a hash, golden bytes, a derived KindId number). A symmetric derive-only roundtrip, a name/schema-shape mirror, or a registry re-test is junk however much ceremony surrounds it.
Policy anchors: CLAUDE.md and docs/guide/testing.md.`

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

// Verbatim refutePrompt from review.js, correctness lens, NO_BUILD grounding.
function refutePrompt(file, fd) {
  const grounding = `\nGROUND THE VERDICT IN TESTS, not inspection — a confident reading of subtle code (math conventions, sign order, edge cases) is exactly where this lens hallucinates a bug.
1. Read the existing #[test]s for this item. A passing test that pins the claimed-broken behavior REFUTES the finding (final_verdict='false-positive') — cite the test by name.
2. No test covers the claim and no build is available here — ground by reading the existing #[test]s and the code path; if a high-severity claim has no covering test, state that in the rationale and return final_verdict='uncertain' rather than confirming on inspection alone.
3. NEVER uphold a finding whose suggested fix would break a currently-passing test — check the fix against the suite before confirming.\n`
  return `A code-review finding was raised under the Correctness lens. Decide whether it survives a STRICT bar — do not rescue it with a plausible story, and do not reject a real issue out of conservatism.

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

phase('Refute')
const jobs = []
for (const cell of CELLS) for (const item of ITEMS) jobs.push({ cell, item })
log(`refute sweep: ${CELLS.length} cells x ${ITEMS.length} findings = ${jobs.length} calls`)

const results = await parallel(jobs.map(j => () =>
  agent(refutePrompt(j.item.file, j.item.fd), {
    label: `refute:${j.cell.model}-${j.cell.effort}:${j.item.id}`,
    phase: 'Refute',
    model: j.cell.model,
    effort: j.cell.effort,
    schema: VERDICT,
  }).then(v => ({ cell: `${j.cell.model}:${j.cell.effort}`, id: j.item.id, truth: j.item.truth, verdict: v ? v.final_verdict : null, rationale: v ? v.rationale.slice(0, 300) : null }))
    .catch(e => ({ cell: `${j.cell.model}:${j.cell.effort}`, id: j.item.id, truth: j.item.truth, verdict: null, rationale: String(e).slice(0, 200) }))
))

const rows = results.filter(Boolean)
const cells = {}
for (const r of rows) {
  const c = (cells[r.cell] ||= { keptReal: 0, real: 0, killedFab: 0, fab: 0, uncertain: 0, failed: 0 })
  if (r.verdict === null) { c.failed++; continue }
  if (r.truth === 'real') { c.real++; if (r.verdict === 'confirmed') c.keptReal++ }
  else { c.fab++; if (r.verdict === 'false-positive') c.killedFab++ }
  if (r.verdict === 'uncertain') c.uncertain++
}
return { cells, rows }
