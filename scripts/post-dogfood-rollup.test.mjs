import assert from 'node:assert/strict'
import { mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import {
  isActionable,
  markerVerdict,
  readRollup,
  renderComment,
  renderNoRollupComment,
} from './post-dogfood-rollup.mjs'

test('an unset or unreadable rollup path selects no-rollup mode', async () => {
  assert.equal(await readRollup(''), null)
  const directory = await mkdtemp(join(tmpdir(), 'aether-no-rollup-'))
  try {
    assert.equal(await readRollup(join(directory, 'missing.json')), null)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test('no-rollup mode records an actionable failed attempt', () => {
  const comment = renderNoRollupComment({
    attempt: 2,
    runUrl: 'https://github.com/iamacoffeepot/aether/actions/runs/12345',
  })

  assert.match(
    comment,
    /<!-- aether-dogfood attempts=2 verdict=failed -->/,
  )
  assert.match(
    comment,
    /\[Open the Actions run\]\(https:\/\/github\.com\/iamacoffeepot\/aether\/actions\/runs\/12345\)/,
  )
  assert.doesNotMatch(comment, /evidence viewer|frame\.png/)
})

test('a clean rollup remains green and non-actionable', () => {
  const rollup = {
    succeeded: true,
    buildGreen: true,
    summary: 'The consumer completed the task.',
    artifact: { verdict: 'correct', rationale: 'The frame matches the expected artifact.' },
    friction: { blocker: [], 'missing-primitive': [], papercut: [], 'doc-gap': [] },
    softHolds: [],
  }

  assert.equal(isActionable(rollup), false)
  assert.equal(markerVerdict(rollup), 'green')
  assert.match(
    renderComment(rollup, false, { attempt: 3, runRef: '3088/12345-3' }),
    /<!-- aether-dogfood attempts=3 verdict=green -->/,
  )
})
