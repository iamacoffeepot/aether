#!/usr/bin/env node

import { createReadStream } from 'node:fs'
import { readFile, writeFile } from 'node:fs/promises'
import { createInterface } from 'node:readline'
import { pathToFileURL } from 'node:url'

function jsonBytes(value) {
  const encoded = JSON.stringify(value)
  return encoded === undefined ? 0 : Buffer.byteLength(encoded, 'utf8')
}

function collectBlocks(value, role, toolUses, toolResults) {
  if (Array.isArray(value)) {
    for (const item of value) collectBlocks(item, role, toolUses, toolResults)
    return
  }
  if (!value || typeof value !== 'object') return

  const nextRole = value.type === 'assistant' || value.type === 'user'
    ? value.type
    : value.role === 'assistant' || value.role === 'user'
      ? value.role
      : role

  if (value.type === 'tool_use') {
    if (
      nextRole === 'assistant'
      && typeof value.id === 'string'
      && typeof value.name === 'string'
      && value.name.startsWith('mcp__')
      && !toolUses.has(value.id)
    ) {
      toolUses.set(value.id, { tool: value.name, bytesIn: jsonBytes(value.input) })
    }
    return
  }

  if (value.type === 'tool_result') {
    if (nextRole === 'user' && typeof value.tool_use_id === 'string' && !toolResults.has(value.tool_use_id)) {
      toolResults.set(value.tool_use_id, value.content)
    }
    collectBlocks(value.content, null, toolUses, toolResults)
    return
  }

  for (const child of Object.values(value)) collectBlocks(child, nextRole, toolUses, toolResults)
}

export async function tokenTableFromTranscript(transcriptPath) {
  const toolUses = new Map()
  const toolResults = new Map()
  const lines = createInterface({
    input: createReadStream(transcriptPath, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  })

  for await (const line of lines) {
    if (!line.trim()) continue
    try {
      collectBlocks(JSON.parse(line), null, toolUses, toolResults)
    } catch {
      // Stream transcripts can contain partial frames after an interrupted run.
    }
  }

  const byTool = new Map()
  for (const [id, toolUse] of toolUses) {
    const row = byTool.get(toolUse.tool) ?? { tool: toolUse.tool, calls: 0, bytesIn: 0, bytesOut: 0 }
    row.calls += 1
    row.bytesIn += toolUse.bytesIn
    if (toolResults.has(id)) row.bytesOut += jsonBytes(toolResults.get(id))
    byTool.set(toolUse.tool, row)
  }

  const tokensPerTool = Array.from(byTool.values(), (row) => ({
    ...row,
    tokensIn: Math.round(row.bytesIn / 4),
    tokensOut: Math.round(row.bytesOut / 4),
  })).sort((left, right) => left.tool.localeCompare(right.tool))

  return { tokensPerTool }
}

export async function enrichRollupFromTranscript(transcriptPath, rollupPath) {
  const tokenTable = await tokenTableFromTranscript(transcriptPath)
  const raw = JSON.parse(await readFile(rollupPath, 'utf8'))
  const rollup = raw && raw.rollup ? raw.rollup : raw
  if (!rollup || typeof rollup !== 'object' || Array.isArray(rollup)) {
    throw new Error('dogfood-token-table: rollup must be an object')
  }

  rollup.tokensPerTool = tokenTable.tokensPerTool
  await writeFile(rollupPath, `${JSON.stringify(raw, null, 2)}\n`)
  return tokenTable
}

async function main() {
  const [transcriptPath, rollupPath] = process.argv.slice(2)
  if (!transcriptPath) {
    throw new Error('usage: dogfood-token-table.mjs <transcript.jsonl> [rollup.json]')
  }

  if (rollupPath) {
    await enrichRollupFromTranscript(transcriptPath, rollupPath)
  } else {
    process.stdout.write(`${JSON.stringify(await tokenTableFromTranscript(transcriptPath), null, 2)}\n`)
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error.message)
    process.exitCode = 1
  })
}
