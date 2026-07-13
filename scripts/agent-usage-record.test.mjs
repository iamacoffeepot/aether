import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseTranscript, buildRecord } from './agent-usage-record.mjs';

const jsonl = (events) => events.map((e) => JSON.stringify(e)).join('\n') + '\n';

const assistant = (model, usage) => ({ type: 'assistant', message: { model, usage } });
const RESULT = {
  type: 'result',
  subtype: 'success',
  is_error: false,
  duration_ms: 337616,
  num_turns: 30,
  total_cost_usd: 4.85,
  usage: {
    input_tokens: 120,
    cache_creation_input_tokens: 5000,
    cache_creation: { ephemeral_1h_input_tokens: 4000, ephemeral_5m_input_tokens: 1000 },
    cache_read_input_tokens: 5070769,
    output_tokens: 27251,
  },
};

const ENVELOPE = {
  task: 'scope', ref: '3145', run_id: '29259531698',
  conclusion: 'success', model: 'opus', created_at: '2026-07-12T00:00:00Z',
};

test('parseTranscript picks the terminal result and the first non-haiku call', () => {
  const t = parseTranscript(jsonl([
    assistant('claude-haiku-4-5', { input_tokens: 999_999, cache_read_input_tokens: 1 }),
    assistant('claude-opus-4-8', { input_tokens: 40, cache_read_input_tokens: 26000, cache_creation_input_tokens: 300 }),
    assistant('claude-opus-4-8', { input_tokens: 5, cache_read_input_tokens: 99999 }),
    RESULT,
  ]));
  assert.equal(t.result.total_cost_usd, 4.85);
  // The haiku call is skipped; the first non-haiku assistant call wins.
  assert.equal(t.firstMain.model, 'claude-opus-4-8');
  assert.equal(t.firstMain.usage.cache_read_input_tokens, 26000);
  assert.equal(t.firstMain.usage.cache_creation_input_tokens, 300);
});

test('buildRecord flattens the cache-class columns and keeps the result whole', () => {
  const transcript = parseTranscript(jsonl([
    assistant('claude-opus-4-8', { input_tokens: 40, cache_read_input_tokens: 26000, cache_creation_input_tokens: 300 }),
    RESULT,
  ]));
  const r = buildRecord({ transcript, envelope: ENVELOPE, pool: null });
  // envelope threaded through verbatim
  assert.equal(r.task, 'scope');
  assert.equal(r.ref, '3145');
  assert.equal(r.run_id, '29259531698');
  assert.equal(r.conclusion, 'success');
  assert.equal(r.model, 'opus');
  // flattened columns off result.usage + cache_creation split
  assert.equal(r.cost_usd, 4.85);
  assert.equal(r.num_turns, 30);
  assert.equal(r.duration_ms, 337616);
  assert.equal(r.input, 120);
  assert.equal(r.cache_write, 5000);
  assert.equal(r.cache_write_1h, 4000);
  assert.equal(r.cache_write_5m, 1000);
  assert.equal(r.cache_read, 5070769);
  assert.equal(r.output, 27251);
  // first main-model call read/write — the warm-resume hit signal
  assert.equal(r.first_call_model, 'claude-opus-4-8');
  assert.equal(r.first_call_cache_read, 26000);
  assert.equal(r.first_call_cache_write, 300);
  assert.equal(r.first_call_input, 40);
  // the terminal record kept whole so the dollars-vs-tokens choice stays open
  assert.deepEqual(r.result, RESULT);
  assert.equal(r.no_result, undefined);
  assert.equal(r.pool, null);
});

test('buildRecord missing cache_creation split defaults the ephemeral columns to 0', () => {
  const noSplit = { ...RESULT, usage: { ...RESULT.usage, cache_creation: undefined } };
  const transcript = { result: noSplit, firstMain: null };
  const r = buildRecord({ transcript, envelope: ENVELOPE, pool: null });
  assert.equal(r.cache_write_1h, 0);
  assert.equal(r.cache_write_5m, 0);
  // no non-haiku call in the stream → first-call columns are null, not 0
  assert.equal(r.first_call_model, null);
  assert.equal(r.first_call_cache_read, null);
});

test('buildRecord on a transcript with no result emits an envelope-only row', () => {
  // A run that died before the terminal record: legible as a row, cost unknown.
  const transcript = parseTranscript(jsonl([
    assistant('claude-opus-4-8', { input_tokens: 40, cache_read_input_tokens: 26000, cache_creation_input_tokens: 300 }),
  ]));
  const r = buildRecord({ transcript, envelope: ENVELOPE, pool: null });
  assert.equal(r.no_result, true);
  assert.equal(r.cost_usd, undefined);
  assert.equal(r.result, undefined);
  // the envelope and first-call signal still survive
  assert.equal(r.task, 'scope');
  assert.equal(r.first_call_cache_read, 26000);
});

test('buildRecord threads a pool block through untouched', () => {
  const pool = { warm: true, session_id: 'sid-9', repool_verdict: 'yes', retire_reason: null };
  const transcript = { result: RESULT, firstMain: null };
  const r = buildRecord({ transcript, envelope: ENVELOPE, pool });
  assert.deepEqual(r.pool, pool);
});
