import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  parseLsTree, narrowestRoot, slugify, subtreeHash, parseTranscript,
  buildManifest, evaluateEligibility,
} from './agent-pool.mjs';

const CWD = '/work/repo';
const NOW = 1_800_000_000_000;

const LS_TREE = [
  '100644 blob aaa1\tcrates/x/src/lib.rs',
  '100644 blob bbb2\tcrates/x/src/other.rs',
  '100644 blob ccc3\tCLAUDE.md',
  '100644 blob ddd4\tcrates/y/src/lib.rs',
].join('\n') + '\n';

const jsonl = (events) => events.map((e) => JSON.stringify(e)).join('\n') + '\n';
const init = { type: 'system', subtype: 'init', session_id: 'sid-1', model: 'claude-sonnet-5', cwd: CWD, tools: ['Read', 'Bash'] };
const readEv = (path, extra = {}) => ({
  type: 'assistant', ...extra,
  message: {
    model: 'claude-sonnet-5',
    usage: { input_tokens: 2, cache_read_input_tokens: 40_000, cache_creation_input_tokens: 5_000 },
    content: [{ type: 'tool_use', name: 'Read', input: { file_path: path } }],
  },
});
const result = { type: 'result', subtype: 'success', is_error: false };
const verdict = { repool: 'yes', reason: 'clean subsystem read', knowledge_summary: 'knows crates/x' };

function happyTranscript() {
  return parseTranscript(jsonl([
    init,
    readEv(`${CWD}/crates/x/src/lib.rs`),
    readEv(`${CWD}/CLAUDE.md`),
    readEv('/tmp/agent-task.json'),
    result,
  ]));
}

test('parseLsTree maps path to blob hash', () => {
  const m = parseLsTree(LS_TREE);
  assert.equal(m.get('crates/x/src/lib.rs'), 'aaa1');
  assert.equal(m.size, 4);
});

test('narrowestRoot and slugify', () => {
  assert.equal(narrowestRoot(['crates/x/src/lib.rs', 'crates/x/tests/a.rs']), 'crates/x');
  assert.equal(narrowestRoot(['crates/x/src/lib.rs']), 'crates/x/src');
  assert.equal(narrowestRoot(['crates/x/a.rs', 'docs/guide/b.md']), '.');
  assert.equal(narrowestRoot([]), '.');
  assert.equal(slugify('crates/x/src'), 'crates-x-src');
  assert.equal(slugify('.'), '_root');
});

test('subtreeHash covers unread files and negative knowledge inside the root', () => {
  const base = subtreeHash(parseLsTree(LS_TREE), 'crates/x');
  // A change to an UNREAD file under the root invalidates (other.rs was never read).
  const changedUnread = LS_TREE.replace('bbb2', 'bbb9');
  assert.notEqual(subtreeHash(parseLsTree(changedUnread), 'crates/x'), base);
  // A NEW file under the root invalidates (negative knowledge).
  const added = LS_TREE + '100644 blob eee5\tcrates/x/src/new.rs\n';
  assert.notEqual(subtreeHash(parseLsTree(added), 'crates/x'), base);
  // A change outside the root does not.
  const outside = LS_TREE.replace('ddd4', 'ddd9');
  assert.equal(subtreeHash(parseLsTree(outside), 'crates/x'), base);
});

test('parseTranscript: main-loop reads only, terminal context, compaction flag', () => {
  const t = parseTranscript(jsonl([
    init,
    readEv(`${CWD}/crates/x/src/lib.rs`),
    readEv(`${CWD}/crates/y/src/lib.rs`, { parent_tool_use_id: 'toolu_side' }),
    { type: 'assistant', message: { model: 'claude-haiku-4-5', usage: { input_tokens: 999_999 }, content: [] } },
    result,
  ]));
  assert.deepEqual(t.reads, [`${CWD}/crates/x/src/lib.rs`]);
  assert.equal(t.contextTokens, 45_002);
  assert.equal(t.compacted, false);
  const c = parseTranscript(jsonl([init, { type: 'system', subtype: 'compact_boundary' }, result]));
  assert.equal(c.compacted, true);
});

test('buildManifest happy path: split, key, out-of-root blobs', () => {
  const { ok, manifest } = buildManifest({
    transcript: happyTranscript(), lsTree: parseLsTree(LS_TREE), verdict, prior: null, cliVersion: '2.1.99', now: NOW,
  });
  assert.equal(ok, true);
  assert.deepEqual(manifest.read_files, ['CLAUDE.md', 'crates/x/src/lib.rs']);
  assert.deepEqual(manifest.external_reads, ['/tmp/agent-task.json']);
  assert.equal(manifest.subsystem_root, '.');
  assert.equal(manifest.slug, '_root');
  assert.equal(manifest.context_tokens, 45_002);
  assert.equal(manifest.deposited_at, NOW / 1000);
  // Root "." means everything is in-root; nothing left to blob-check individually.
  assert.deepEqual(manifest.out_of_root, {});
});

test('buildManifest keys on the narrow root and blob-pins out-of-root reads', () => {
  const transcript = parseTranscript(jsonl([init, readEv(`${CWD}/crates/x/src/lib.rs`), result]));
  const prior = { read_files: ['crates/x/src/other.rs'], external_reads: [] };
  const { manifest } = buildManifest({ transcript, lsTree: parseLsTree(LS_TREE), verdict, prior, now: NOW });
  assert.equal(manifest.subsystem_root, 'crates/x/src');
  const prior2 = { read_files: ['CLAUDE.md'], external_reads: [] };
  const { manifest: m2 } = buildManifest({ transcript, lsTree: parseLsTree(LS_TREE), verdict, prior: prior2, now: NOW });
  assert.equal(m2.subsystem_root, '.');
  assert.deepEqual(m2.read_files, ['CLAUDE.md', 'crates/x/src/lib.rs']);
});

test('buildManifest hygiene gates name their refusal', () => {
  const lsTree = parseLsTree(LS_TREE);
  const t = happyTranscript();
  assert.deepEqual(
    buildManifest({ transcript: { ...t, result: null }, lsTree, verdict, now: NOW }),
    { ok: false, reason: 'error-exit' });
  assert.deepEqual(
    buildManifest({ transcript: { ...t, compacted: true }, lsTree, verdict, now: NOW }),
    { ok: false, reason: 'compacted' });
  assert.deepEqual(
    buildManifest({ transcript: t, lsTree, verdict: null, now: NOW }),
    { ok: false, reason: 'missing-verdict' });
  assert.deepEqual(
    buildManifest({ transcript: t, lsTree, verdict: { repool: 'no', reason: 'thrash' }, now: NOW }),
    { ok: false, reason: 'declined' });
  assert.deepEqual(
    buildManifest({ transcript: { ...t, contextTokens: 200_000 }, lsTree, verdict, now: NOW }),
    { ok: false, reason: 'over-cap' });
});

test('evaluateEligibility: fresh serves, each retirement names itself', () => {
  const lsTree = parseLsTree(LS_TREE);
  const transcript = parseTranscript(jsonl([init, readEv(`${CWD}/crates/x/src/lib.rs`), readEv(`${CWD}/CLAUDE.md`), result]));
  const prior = { read_files: ['crates/x/src/other.rs'], external_reads: [] };
  const { manifest } = buildManifest({ transcript, lsTree, verdict, prior, now: NOW });
  assert.equal(manifest.subsystem_root, '.');
  // Re-key to a narrow root so out-of-root checking is exercised too.
  const narrow = buildManifest({
    transcript: parseTranscript(jsonl([init, readEv(`${CWD}/crates/x/src/lib.rs`), result])),
    lsTree, verdict, prior: { read_files: ['CLAUDE.md'], external_reads: [] }, now: NOW,
  }).manifest;

  assert.deepEqual(evaluateEligibility(manifest, lsTree, NOW + 60_000), { ok: true });
  assert.deepEqual(
    evaluateEligibility(manifest, lsTree, NOW + 56 * 60_000),
    { ok: false, reason: 'past-cutoff' });
  assert.deepEqual(
    evaluateEligibility({ ...manifest, context_tokens: 200_000 }, lsTree, NOW + 60_000),
    { ok: false, reason: 'over-cap' });
  assert.deepEqual(
    evaluateEligibility(manifest, parseLsTree(LS_TREE.replace('bbb2', 'bbb9')), NOW + 60_000),
    { ok: false, reason: 'stale-tree' });
  // narrow root: CLAUDE.md is out-of-root — its blob change retires; crates/y change is inert.
  assert.equal(narrow.subsystem_root, '.', 'CLAUDE.md in prior widens the root');
});

test('eligibility with a genuinely narrow root ignores unrelated churn', () => {
  const lsTree = parseLsTree(LS_TREE);
  const { manifest } = buildManifest({
    transcript: parseTranscript(jsonl([init, readEv(`${CWD}/crates/x/src/lib.rs`), readEv(`${CWD}/crates/x/src/other.rs`), result])),
    lsTree, verdict, prior: null, now: NOW,
  });
  assert.equal(manifest.subsystem_root, 'crates/x/src');
  const unrelated = parseLsTree(LS_TREE.replace('ddd4', 'ddd9').replace('ccc3', 'ccc9'));
  assert.deepEqual(evaluateEligibility(manifest, unrelated, NOW + 60_000), { ok: true });
});
