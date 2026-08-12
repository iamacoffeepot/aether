import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

import { withinTrailingWindow, deterministicSample, parseClosingIssue, isCodeBearing, parsePlanRouting } from './quality-eval-select.mjs'
import { parseVerdict, aggregateRates, renderRollup } from './quality-eval-judge.mjs'

const DAY = 86_400_000
const NOW = 1_700_000_000_000 // fixed "now" so the window math is deterministic

test('withinTrailingWindow keeps merged PRs inside the window and drops the rest', () => {
  const prs = [
    { number: 1, merged_at: new Date(NOW - 1 * DAY).toISOString() }, // in
    { number: 2, merged_at: new Date(NOW - 6 * DAY).toISOString() }, // in
    { number: 3, merged_at: new Date(NOW - 8 * DAY).toISOString() }, // out (too old)
    { number: 4, merged_at: null }, // out (closed, never merged)
    { number: 5, merged_at: new Date(NOW + 1 * DAY).toISOString() }, // out (future)
  ]
  const kept = withinTrailingWindow(prs, NOW, 7).map((p) => p.number)
  assert.deepEqual(kept, [1, 2])
})

test('deterministicSample is stable across input order and caps at n', () => {
  const items = [{ id: 'a' }, { id: 'b' }, { id: 'c' }, { id: 'd' }, { id: 'e' }]
  const key = (x) => x.id
  const first = deterministicSample(items, 3, key)
  const shuffled = deterministicSample([...items].reverse(), 3, key)
  assert.equal(first.length, 3)
  // Same candidate set -> same sample regardless of input order (re-runnable).
  assert.deepEqual(first.map(key), shuffled.map(key))
  // n >= length returns every item.
  assert.equal(deterministicSample(items, 99, key).length, items.length)
})

test('parseClosingIssue reads the linked issue, or null', () => {
  assert.equal(parseClosingIssue('Closes #1234.\n\n## Summary'), 1234)
  assert.equal(parseClosingIssue('fixes #7'), 7)
  assert.equal(parseClosingIssue('Resolves #42 in passing'), 42)
  assert.equal(parseClosingIssue('no reference here'), null)
})

test('isCodeBearing requires a crate source file', () => {
  assert.equal(isCodeBearing(['crates/aether-data/src/lib.rs']), true)
  assert.equal(isCodeBearing(['docs/guide/x.md', '.github/workflows/ci.yml']), false)
  assert.equal(isCodeBearing([]), false)
})

const VALID_PLAN = `## Implementation plan

1. Make the bounded change.

**Size:** m
**Implementation model:** sonnet
**Routing reason:** One focused parser and its tests.

## Declared surface

\`\`\`
scripts/example.mjs
\`\`\``

test('parsePlanRouting returns the normalized final Plan routing', () => {
  assert.deepEqual(parsePlanRouting(VALID_PLAN), { size: 'm', model: 'sonnet' })
  assert.deepEqual(
    parsePlanRouting(
      VALID_PLAN.replace('**Size:** m', '**Size:** s\n').replace('**Implementation model:** sonnet', '**Implementation model:** haiku\n'),
    ),
    { size: 's', model: 'haiku' },
  )
})

test('parsePlanRouting rejects missing and duplicated managed Plan sections', () => {
  assert.equal(parsePlanRouting('## Description\n\nNo Plan here.'), null)
  assert.equal(parsePlanRouting(`${VALID_PLAN}\n\n## Implementation plan\n\n**Size:** l\n**Implementation model:** opus\n**Routing reason:** Duplicate.`), null)
})

test('parsePlanRouting rejects duplicated, malformed, and misplaced routing', () => {
  assert.equal(parsePlanRouting(VALID_PLAN.replace('1. Make the bounded change.', '1. Make the bounded change.\n\n**Size:** s')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Size:** m', '**Size:** medium')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Size:** m', ' **Size:** m')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Size:** m', '**Size:**  m')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Implementation model:** sonnet', '**Implementation model:** gpt-5')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Routing reason:** One focused parser and its tests.', '**Routing reason:**')), null)
  assert.equal(parsePlanRouting(VALID_PLAN.replace('**Routing reason:** One focused parser and its tests.', '**Routing reason:** One focused parser and its tests.\n\n4. Not final.')), null)
})

test('parseVerdict extracts verdict + defect_class, ignoring class on correct', () => {
  assert.deepEqual(parseVerdict('```quality-verdict\nverdict: correct\ndefect_class: none\n```'), {
    verdict: 'correct',
    defect_class: null,
  })
  assert.deepEqual(parseVerdict('verdict: defect\ndefect_class: boundary'), {
    verdict: 'defect',
    defect_class: 'boundary',
  })
  // A defect verdict whose class reads "none" carries no class.
  assert.deepEqual(parseVerdict('verdict: defect\ndefect_class: -'), { verdict: 'defect', defect_class: null })
  // Unparseable -> unknown, never scored as correct.
  assert.deepEqual(parseVerdict('the model rambled'), { verdict: 'unknown', defect_class: null })
})

// Tripwire: the rate aggregation is the harness's whole output — a regression
// alarm reads these numbers to decide whether routing degraded correctness. The
// defect_rate is computed over SCORED (correct+defect) samples only; an
// `unknown` verdict is counted in `total`/`unknown` but must never move the
// rate. If the grouping key, the scored denominator, or the overall roll-up
// ever drifts, these fixed rates change and this test catches it.
test('aggregateRates groups by size/model and rates over scored samples only', () => {
  const verdicts = [
    { issue: 1, verdict: 'correct', size: 'l', model: 'opus' },
    { issue: 2, verdict: 'defect', size: 'l', model: 'opus' },
    { issue: 3, verdict: 'correct', size: 'm', model: 'sonnet' },
    { issue: 4, verdict: 'unknown', size: 'm', model: 'sonnet' },
  ]
  const { rows, overall } = aggregateRates(verdicts)

  const lOpus = rows.find((r) => r.size === 'l' && r.model === 'opus')
  assert.deepEqual(
    { total: lOpus.total, correct: lOpus.correct, defect: lOpus.defect, defect_rate: lOpus.defect_rate },
    { total: 2, correct: 1, defect: 1, defect_rate: 0.5 },
  )

  const mSonnet = rows.find((r) => r.size === 'm' && r.model === 'sonnet')
  // One correct + one unknown: scored denominator is 1, so defect_rate is 0 — the
  // unknown neither scores as correct nor inflates the defect rate.
  assert.deepEqual(
    { total: mSonnet.total, correct: mSonnet.correct, unknown: mSonnet.unknown, defect_rate: mSonnet.defect_rate },
    { total: 2, correct: 1, unknown: 1, defect_rate: 0 },
  )

  assert.equal(overall.total, 4)
  assert.equal(overall.correct, 2)
  assert.equal(overall.defect, 1)
  assert.equal(overall.defect_rate, 1 / 3) // 1 defect over 3 scored (correct+defect)
})

test('aggregateRates reports a null rate when no sample is scorable', () => {
  const { overall } = aggregateRates([{ issue: 9, verdict: 'unknown', size: 's', model: 'opus' }])
  assert.equal(overall.defect_rate, null)
})

test('renderRollup carries normalized size/model fields into the judge report', () => {
  const rollup = renderRollup([{ issue: 11, verdict: 'defect', defect_class: 'boundary', size: 's', model: 'haiku' }])
  assert.match(rollup, /\| s \| haiku \| 1 \| 0 \| 1 \| 100% \|/)
  assert.match(rollup, /#11 \(s \/ haiku\) — boundary/)
  assert.doesNotMatch(rollup, /size:s|model:haiku/)
})

test('quality-eval-run preserves the normalized routing fields and model alias', (t) => {
  const root = mkdtempSync(join(tmpdir(), 'quality-eval-run-test-'))
  const bin = join(root, 'bin')
  const home = join(root, 'home')
  const issue = process.pid
  const agentLog = `/tmp/quality-eval-agent-${issue}.jsonl`
  mkdirSync(bin)
  mkdirSync(home)
  writeFileSync(
    join(bin, 'git'),
    `#!/usr/bin/env bash
if [[ "\${1:-}" == clone ]]; then
  mkdir -p "\${@: -1}"
  exit 0
fi
if [[ "\${1:-}" == -C ]]; then
  case "\${3:-}" in
    rev-list) printf '%s\\n' "$QUALITY_TEST_PARENT_SHA" ;;
    diff) printf 'candidate diff' ;;
    show) printf 'landed diff' ;;
  esac
fi
exit 0
`,
  )
  writeFileSync(join(bin, 'timeout'), '#!/usr/bin/env bash\nshift\nexec "$@"\n')
  writeFileSync(join(bin, 'claude'), '#!/usr/bin/env bash\nexit 0\n')
  chmodSync(join(bin, 'git'), 0o755)
  chmodSync(join(bin, 'timeout'), 0o755)
  chmodSync(join(bin, 'claude'), 0o755)

  t.after(() => {
    rmSync(root, { recursive: true, force: true })
    rmSync(agentLog, { force: true })
  })

  const input = `${JSON.stringify({
    issue,
    parent_sha: '1'.repeat(40),
    squash_sha: '2'.repeat(40),
    model: 'haiku',
    size: 's',
    issue_body: 'Implement the issue.',
  })}\n`
  const records = join(root, 'records.jsonl')
  writeFileSync(records, input)
  const result = spawnSync('bash', [fileURLToPath(new URL('./quality-eval-run.sh', import.meta.url)), records], {
    encoding: 'utf8',
    env: {
      ...process.env,
      PATH: `${bin}:${process.env.PATH}`,
      HOME: home,
      GITHUB_WORKSPACE: root,
      QUALITY_TEST_PARENT_SHA: '1'.repeat(40),
    },
  })

  assert.equal(result.status, 0, result.stderr)
  assert.notEqual(result.stdout.trim(), '', result.stderr)
  const record = JSON.parse(result.stdout.trim())
  assert.equal(record.model, 'haiku')
  assert.equal(record.size, 's')
  assert.deepEqual(Object.keys(record).sort(), ['candidate_diff', 'issue', 'landed_diff', 'model', 'size'])
  assert.match(result.stderr, /\(model haiku\)/)
})
