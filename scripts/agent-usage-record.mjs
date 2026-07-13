#!/usr/bin/env node
// Per-run usage record for the fleet spend ledger (rung 1, #3166). After a
// headless box finishes, this derives one JSON object from the box's tee'd
// stream-json transcript and prints it to stdout; the workflow step wraps the
// derivation in an `aws s3 cp` to s3://aether-evidence/agent/usage/<day>/<run>.
// One object per run is race-free — each run owns a distinct key, so concurrent
// waves never contend and no read-modify-write append is needed.
//
// What it records, and why more than the terminal `result` alone:
//   - the entire terminal `result` record (cost, turns, duration, usage) — kept
//     whole so rung 2's dollars-vs-tokens denomination decision stays open;
//   - flattened cache-class columns (uncached input, the cache-write split
//     ephemeral_1h / ephemeral_5m, cache-read, output) — the ephemeral_5m bucket
//     lighting up is the direct signal the provider TTL dropped to the 5m class;
//   - the first main-model call's read/write/input — the cross-box prompt-cache
//     hit signal the warm pool (#3264) is metered against.
// The split and the first-call values live in per-call records of the stream,
// so the record is derived over the whole transcript, not the `result` alone.
// Column derivations are seeded from the spike's extract.py (frozen
// spike/session-pool branch).
//
// Guard: a transcript with no terminal `result` (a run that died early) still
// emits an envelope-only record with no_result:true — never exits non-zero, so
// a metering step can `|| true` and a dead run is still legible as a row.
//
// Usage:
//   agent-usage-record.mjs --transcript <jsonl> \
//     --task T --ref R --run-id ID --conclusion C --model M --created-at TS \
//     [--pool <json-file>]

import { readFileSync } from 'node:fs';

export function parseTranscript(text) {
  // The two facts the record needs beyond the envelope: the terminal `result`
  // record, and the first main-model (non-haiku) assistant call's usage — the
  // warm-resume cache-hit signal. Faithful to extract.py: first assistant
  // usage on any non-haiku model, whichever comes first in the stream.
  let result = null;
  let firstMain = null;
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let ev;
    try { ev = JSON.parse(line); } catch { continue; }
    if (ev.type === 'result') {
      result = ev;
    } else if (ev.type === 'assistant' && !firstMain) {
      const msg = ev.message || {};
      if (/haiku/.test(msg.model || '')) continue;
      if (msg.usage) firstMain = { model: msg.model || null, usage: msg.usage };
    }
  }
  return { result, firstMain };
}

export function buildRecord({ transcript, envelope, pool }) {
  const { result, firstMain } = transcript;
  const record = {
    schema: 1,
    task: envelope.task ?? null,
    ref: envelope.ref ?? null,
    run_id: envelope.run_id ?? null,
    conclusion: envelope.conclusion ?? null,
    model: envelope.model ?? null,
    created_at: envelope.created_at ?? null,
    pool: pool ?? null,
  };

  if (firstMain) {
    const fu = firstMain.usage;
    record.first_call_model = firstMain.model;
    record.first_call_cache_read = fu.cache_read_input_tokens ?? 0;
    record.first_call_cache_write = fu.cache_creation_input_tokens ?? 0;
    record.first_call_input = fu.input_tokens ?? 0;
  } else {
    record.first_call_model = null;
    record.first_call_cache_read = null;
    record.first_call_cache_write = null;
    record.first_call_input = null;
  }

  if (!result) {
    // A run that died before the terminal record — legible as a row, cost
    // unknown. Emit what is derivable (the envelope + first-call), never fail.
    record.no_result = true;
    return record;
  }

  const usage = result.usage || {};
  const cc = usage.cache_creation || {};
  record.num_turns = result.num_turns ?? null;
  record.cost_usd = result.total_cost_usd ?? null;
  record.duration_ms = result.duration_ms ?? null;
  record.is_error = result.is_error ?? null;
  record.input = usage.input_tokens ?? 0;
  record.cache_write = usage.cache_creation_input_tokens ?? 0;
  record.cache_write_1h = cc.ephemeral_1h_input_tokens ?? 0;
  record.cache_write_5m = cc.ephemeral_5m_input_tokens ?? 0;
  record.cache_read = usage.cache_read_input_tokens ?? 0;
  record.output = usage.output_tokens ?? 0;
  // The terminal record whole — keeps every meter on disk for rung 2.
  record.result = result;
  return record;
}

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? undefined : process.argv[i + 1];
}

function main() {
  const transcriptPath = arg('transcript');
  let transcript = { result: null, firstMain: null };
  try {
    transcript = parseTranscript(readFileSync(transcriptPath, 'utf8'));
  } catch { /* unreadable/missing transcript → envelope-only record */ }

  let pool = null;
  const poolPath = arg('pool');
  if (poolPath) {
    try { pool = JSON.parse(readFileSync(poolPath, 'utf8')); } catch { /* no pool block */ }
  }

  const envelope = {
    task: arg('task'),
    ref: arg('ref'),
    run_id: arg('run-id'),
    conclusion: arg('conclusion'),
    model: arg('model'),
    created_at: arg('created-at'),
  };

  console.log(JSON.stringify(buildRecord({ transcript, envelope, pool })));
}

if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) main();
