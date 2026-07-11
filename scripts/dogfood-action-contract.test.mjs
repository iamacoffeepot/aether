import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'

const DOGFOOD_WORKFLOW = new URL('../.github/workflows/dogfood.yml', import.meta.url)

test('a failed no-rollup trial reaches the writer only after resolve succeeds', async () => {
  const workflow = await readFile(DOGFOOD_WORKFLOW, 'utf8')

  assert.match(
    workflow,
    /if: always\(\) && needs\.resolve\.result == 'success' && \(needs\.trial\.outputs\.rollup_produced == 'true' \|\| needs\.trial\.result == 'failure'\)/,
  )
  assert.match(
    workflow,
    /- name: Account MCP tool tokens\n        if: needs\.trial\.outputs\.rollup_produced == 'true'/,
  )
  assert.match(workflow, /else\n            node scripts\/post-dogfood-rollup\.mjs/)
  assert.match(workflow, /ROLLUP_PATH=dogfood-run\/rollup\.json HAS_FRAME="\$HAS_FRAME"/)
  assert.match(workflow, /RUN_URL: \$\{\{ github\.server_url \}\}\/\$\{\{ github\.repository \}\}\/actions\/runs\/\$\{\{ github\.run_id \}\}/)
  assert.match(workflow, /do NOT fabricate a\n          replacement file or claim success/)
  assert.match(
    workflow,
    /- name: Push the run to the evidence branch\n        if: needs\.trial\.outputs\.rollup_produced == 'true'/,
  )
  assert.match(workflow, /else\n            actionable=true/)
  assert.doesNotMatch(workflow, /- name: Bounce the issue at the attempt cap\n        if:/)
  assert.doesNotMatch(workflow, /Record a no-rollup failure/)
})

test('a produced rollup keeps the existing evidence, token, and poster path', async () => {
  const workflow = await readFile(DOGFOOD_WORKFLOW, 'utf8')

  assert.match(
    workflow,
    /- uses: actions\/download-artifact@v4\n        if: needs\.trial\.outputs\.rollup_produced == 'true'\n        with:\n          name: dogfood-run/,
  )
  assert.match(workflow, /node scripts\/dogfood-token-table\.mjs dogfood-run\/transcript\.jsonl dogfood-run\/rollup\.json/)
  assert.match(workflow, /HAS_FRAME="\$HAS_FRAME" node scripts\/post-dogfood-rollup\.mjs/)
})
