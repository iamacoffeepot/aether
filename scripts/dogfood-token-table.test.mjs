import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'

import { enrichRollupFromTranscript, tokenTableFromTranscript } from './dogfood-token-table.mjs'

const FIXTURE = new URL('./fixtures/dogfood-token-table/transcript.jsonl', import.meta.url)

test('accounts observed nested MCP tool calls and exact result shapes', async () => {
  assert.deepEqual(await tokenTableFromTranscript(FIXTURE), {
    tokensPerTool: [
      {
        tool: 'mcp__aether-hub__capture_frame',
        calls: 2,
        bytesIn: 61,
        bytesOut: 125,
        tokensIn: 15,
        tokensOut: 31,
      },
      {
        tool: 'mcp__aether-hub__list_engines',
        calls: 1,
        bytesIn: 2,
        bytesOut: 18,
        tokensIn: 1,
        tokensOut: 5,
      },
    ],
  })
})

test('a failed transcript with no MCP traffic produces an empty table', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'aether-dogfood-token-table-'))
  const transcriptPath = join(directory, 'transcript.jsonl')
  await writeFile(transcriptPath, '{"type":"result","subtype":"error","result":"trial failed"}\n')

  try {
    assert.deepEqual(await tokenTableFromTranscript(transcriptPath), { tokensPerTool: [] })
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test('enriches both wrapped and bare rollups with named records', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'aether-dogfood-token-table-'))

  try {
    for (const fixture of [
      { name: 'wrapped.json', initial: { rollup: { succeeded: true }, task: { issue: 3011 } } },
      { name: 'bare.json', initial: { succeeded: true } },
    ]) {
      const rollupPath = join(directory, fixture.name)
      await writeFile(rollupPath, JSON.stringify(fixture.initial))
      await enrichRollupFromTranscript(FIXTURE, rollupPath)
      const enriched = JSON.parse(await readFile(rollupPath, 'utf8'))
      const rollup = enriched.rollup ?? enriched
      assert.equal(rollup.tokensPerTool.length, 2)
      assert.equal(rollup.tokensPerTool[0].tool, 'mcp__aether-hub__capture_frame')
    }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
