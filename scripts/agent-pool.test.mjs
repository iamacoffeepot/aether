import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  parseLsTree, narrowestRoot, slugify, subtreeHash, parseTranscript,
  buildManifest, evaluateEligibility, prefixDiff, headHash, isHeadInput,
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

test('parseTranscript captures turn-1 resume cost from the first main-loop turn', () => {
  const t = happyTranscript();
  // The first readEv's usage — the resume's turn-1 write/read (#3347).
  assert.equal(t.turn1CacheCreation, 5_000);
  assert.equal(t.turn1CacheRead, 40_000);
});

test('turn-1 capture reads a haiku main-loop turn — before the context-accounting skip', () => {
  const haikuTurn = {
    type: 'assistant',
    message: { model: 'claude-haiku-4-5', usage: { input_tokens: 3, cache_read_input_tokens: 111, cache_creation_input_tokens: 222 }, content: [] },
  };
  const t = parseTranscript(jsonl([init, haikuTurn, result]));
  // Turn-1 is captured regardless of model (the canary probes on haiku)...
  assert.equal(t.turn1CacheCreation, 222);
  assert.equal(t.turn1CacheRead, 111);
  // ...while contextTokens still applies the haiku skip — a separate axis.
  assert.equal(t.contextTokens, 0);
});

test('turn-1 is the FIRST usage-bearing turn on a resume-only transcript', () => {
  const firstTurn = {
    type: 'assistant',
    message: { model: 'claude-sonnet-5', usage: { input_tokens: 1, cache_read_input_tokens: 7, cache_creation_input_tokens: 9 }, content: [] },
  };
  // No init, and a second turn with larger usage follows — turn-1 must be the first (9/7), not the second.
  const t = parseTranscript(jsonl([firstTurn, readEv(`${CWD}/CLAUDE.md`), result]));
  assert.equal(t.turn1CacheCreation, 9);
  assert.equal(t.turn1CacheRead, 7);
});

test('turn-1 is null on a transcript with no main-loop usage', () => {
  const t = parseTranscript(jsonl([init, result]));
  assert.equal(t.turn1CacheCreation, null);
  assert.equal(t.turn1CacheRead, null);
});

test('resume-cost subcommand prints write=<n> read=<n>', () => {
  const dir = mkdtempSync(join(tmpdir(), 'poolcost-'));
  const populated = join(dir, 'warm.jsonl');
  writeFileSync(populated, jsonl([init, readEv(`${CWD}/CLAUDE.md`), result]));
  assert.equal(
    execFileSync('node', ['scripts/agent-pool.mjs', 'resume-cost', '--transcript', populated], { encoding: 'utf8' }).trim(),
    'write=5000 read=40000');
  // Null turn-1 renders as 0/0, never a crash or blank.
  const empty = join(dir, 'cold.jsonl');
  writeFileSync(empty, jsonl([init, result]));
  assert.equal(
    execFileSync('node', ['scripts/agent-pool.mjs', 'resume-cost', '--transcript', empty], { encoding: 'utf8' }).trim(),
    'write=0 read=0');
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

test('evaluateEligibility: fresh serves; cutoff and cap still retire, cap boundary inclusive', () => {
  const lsTree = parseLsTree(LS_TREE);
  const transcript = parseTranscript(jsonl([init, readEv(`${CWD}/crates/x/src/lib.rs`), readEv(`${CWD}/CLAUDE.md`), result]));
  const prior = { read_files: ['crates/x/src/other.rs'], external_reads: [] };
  const { manifest } = buildManifest({ transcript, lsTree, verdict, prior, now: NOW });

  assert.deepEqual(evaluateEligibility(manifest, lsTree, NOW + 60_000), { ok: true });
  assert.deepEqual(
    evaluateEligibility(manifest, lsTree, NOW + 56 * 60_000),
    { ok: false, reason: 'past-cutoff' });
  assert.deepEqual(
    evaluateEligibility({ ...manifest, context_tokens: 200_000 }, lsTree, NOW + 60_000),
    { ok: false, reason: 'over-cap' });
  // Exactly at the cap still serves — the check is a strict `>`.
  assert.deepEqual(
    evaluateEligibility({ ...manifest, context_tokens: 150_000 }, lsTree, NOW + 60_000),
    { ok: true });
});

// Tripwire (#3341): the belief-truth gate — subtree_hash / out_of_root blob
// matching — is retired. A manifest whose in-root subtree AND whose pinned
// out-of-root blobs have BOTH churned since deposit must still serve; that
// combination is exactly what the old `stale-tree` gate retired on every
// non-judge task, which is the regression this issue exists to undo.
// Exercised against both an implement-shaped (crate-scoped root, a pinned
// out-of-root read) and a land-shaped (repo-wide root) manifest so the
// uniform-across-tasks claim isn't just tested for one shape. The head stays
// FRESH here (only belief files churn) so this isolates belief churn from the
// #3422 head-freshness gate — CLAUDE.md, a head input, is deliberately not
// churned; its churn is the next test's subject, not this one's.
test('evaluateEligibility ignores belief-file churn — read-source tree beliefs are not re-checked', () => {
  const lsTree = parseLsTree(LS_TREE);
  const beliefChurned = parseLsTree(LS_TREE.replace('bbb2', 'bbb9').replace('ddd4', 'ddd9'));
  const head = headHash(lsTree);

  const implementShaped = {
    subsystem_root: 'crates/x',
    subtree_hash: subtreeHash(lsTree, 'crates/x'),
    out_of_root: { 'CLAUDE.md': 'ccc3' },
    head_hash: head,
    context_tokens: 10_000,
    deposited_at: NOW / 1000,
  };
  const landShaped = {
    subsystem_root: '.',
    subtree_hash: subtreeHash(lsTree, '.'),
    out_of_root: {},
    head_hash: head,
    context_tokens: 10_000,
    deposited_at: NOW / 1000,
  };

  for (const manifest of [implementShaped, landShaped]) {
    assert.deepEqual(evaluateEligibility(manifest, beliefChurned, NOW + 60_000), { ok: true });
  }
});

test('headHash covers CLAUDE.md and skills, ignores everything else', () => {
  assert.equal(isHeadInput('CLAUDE.md'), true);
  assert.equal(isHeadInput('.claude/skills/land/SKILL.md'), true);
  assert.equal(isHeadInput('crates/x/src/lib.rs'), false);
  assert.equal(isHeadInput('docs/adr/0001-x.md'), false);
  const base = headHash(parseLsTree(LS_TREE));
  // A non-head (belief-source) file moving does not change the head hash...
  assert.equal(headHash(parseLsTree(LS_TREE.replace('aaa1', 'aaa9'))), base);
  // ...but CLAUDE.md moving does.
  assert.notEqual(headHash(parseLsTree(LS_TREE.replace('ccc3', 'ccc9'))), base);
});

// Tripwire (#3422): the head-freshness gate. The static head (CLAUDE.md +
// skills) is the cached prefix a warm resume reuses, NOT a re-derived belief,
// so a head that moved on origin/main since deposit (head_hash mismatch) is a
// real cache miss and must retire the entry — the deterministic in-repo half
// of the resume cache miss. Distinct from the belief-churn test above: there
// the head is held fresh and churn is ignored; here the head itself moves.
test('evaluateEligibility retires on head drift — CLAUDE.md or a skill moved since deposit', () => {
  const withSkill = LS_TREE + '100644 blob sk01\t.claude/skills/land/SKILL.md\n';
  const lsTree = parseLsTree(withSkill);
  const manifest = { context_tokens: 10_000, deposited_at: NOW / 1000, head_hash: headHash(lsTree) };

  // A fresh head serves.
  assert.deepEqual(evaluateEligibility(manifest, lsTree, NOW + 60_000), { ok: true });
  // CLAUDE.md's blob moving retires.
  assert.deepEqual(
    evaluateEligibility(manifest, parseLsTree(withSkill.replace('ccc3', 'ccc9')), NOW + 60_000),
    { ok: false, reason: 'head-drift' });
  // A skill file's blob moving retires.
  assert.deepEqual(
    evaluateEligibility(manifest, parseLsTree(withSkill.replace('sk01', 'sk99')), NOW + 60_000),
    { ok: false, reason: 'head-drift' });
  // A belief-source file moving with the head held fresh still serves — the gate is head-scoped.
  assert.deepEqual(
    evaluateEligibility(manifest, parseLsTree(withSkill.replace('bbb2', 'bbb9')), NOW + 60_000),
    { ok: true });
});

test('evaluateEligibility skips the head gate for a pre-gate manifest carrying no head_hash', () => {
  // A manifest deposited before the #3422 gate has no head_hash; it is not
  // retired on head drift (it ages out within the cutoff), keeping the #3341
  // don't-retire-what-you-can't-prove-stale posture for legacy entries.
  const legacy = { context_tokens: 10_000, deposited_at: NOW / 1000 };
  const headMoved = parseLsTree(LS_TREE.replace('ccc3', 'ccc9'));
  assert.deepEqual(evaluateEligibility(legacy, headMoved, NOW + 60_000), { ok: true });
});

test('buildManifest records a head_hash that gates its own fresh-checkout eligibility', () => {
  const lsTree = parseLsTree(LS_TREE);
  const { manifest } = buildManifest({ transcript: happyTranscript(), lsTree, verdict, now: NOW });
  assert.equal(manifest.head_hash, headHash(lsTree));
  // A fresh checkout serves; the head moving under it retires.
  assert.deepEqual(evaluateEligibility(manifest, lsTree, NOW + 60_000), { ok: true });
  assert.deepEqual(
    evaluateEligibility(manifest, parseLsTree(LS_TREE.replace('ccc3', 'ccc9')), NOW + 60_000),
    { ok: false, reason: 'head-drift' });
});

test('prefixDiff: a clean prefix (deposited wholly at the front of resuming) is the hit case', () => {
  // The resuming transcript is the replayed prior turns PLUS new turns, so the
  // deposited transcript is a byte-identical prefix — the byte-stable resume.
  const deposited = jsonl([init, readEv(`${CWD}/CLAUDE.md`)]);
  const resuming = deposited + jsonl([readEv(`${CWD}/crates/x/src/lib.rs`), result]);
  const r = prefixDiff(deposited, resuming);
  assert.equal(r.identical, true);
  assert.equal(r.cleanPrefix, true);
  assert.equal(r.sharedBytes, Buffer.byteLength(deposited));
  assert.equal(r.depositedBytes, Buffer.byteLength(deposited));
  assert.equal(r.resumingBytes, Buffer.byteLength(resuming));
});

test('prefixDiff: shared region matches but deposited runs past resuming — not a clean prefix', () => {
  const resuming = jsonl([init, readEv(`${CWD}/CLAUDE.md`)]);
  const deposited = resuming + jsonl([result]);
  const r = prefixDiff(deposited, resuming);
  assert.equal(r.identical, true);
  assert.equal(r.cleanPrefix, false);
  assert.equal(r.sharedBytes, Buffer.byteLength(resuming));
});

test('prefixDiff: a byte drift inside the shared region localizes to its enclosing event', () => {
  // Identical init line; the second (assistant) event diverges — the first
  // differing byte falls inside event index 1.
  const deposited = '{"type":"system","subtype":"init"}\n{"type":"assistant","x":1}\n';
  const resuming = '{"type":"system","subtype":"init"}\n{"type":"assistant","x":2}\n';
  const r = prefixDiff(deposited, resuming);
  assert.equal(r.identical, false);
  assert.equal(r.offset, deposited.indexOf('1'));
  assert.equal(r.eventIndex, 1);
  assert.deepEqual(r.event, { type: 'assistant', subtype: null });
  // The byte offset within its own line, and the divergent-byte windows differ.
  assert.equal(r.byteInLine, deposited.indexOf('1') - deposited.indexOf('\n') - 1);
  assert.notEqual(r.deposited, r.resuming);
});

// Tripwire (#3422): the diff is on UTF-8 BYTES, not UTF-16 code units — the
// warm-resume miss is measured against the cached byte prefix, and the whole
// point of the localizer is a byte offset that lines up with the API's cache
// accounting. A multibyte char before the divergence pushes the byte offset
// past the string index, so a regression to string comparison would flip this.
test('prefixDiff reports a BYTE offset, not a UTF-16 code-unit index', () => {
  const prefix = '{"m":"—"}\n'; // em-dash — 3 UTF-8 bytes, 1 code unit
  const deposited = prefix + '{"x":"a"}\n';
  const resuming = prefix + '{"x":"b"}\n';
  const r = prefixDiff(deposited, resuming);
  assert.equal(r.identical, false);
  // The byte offset counts the em-dash as 3 bytes; the string index counts 1.
  assert.equal(r.offset, Buffer.byteLength(deposited.slice(0, deposited.indexOf('a'))));
  assert.notEqual(r.offset, deposited.indexOf('a'));
});

test('prefix-diff subcommand reads transcripts as raw buffers and prints the JSON verdict', () => {
  const dir = mkdtempSync(join(tmpdir(), 'poolpfx-'));
  const dep = join(dir, 'deposited.jsonl');
  const res = join(dir, 'resuming.jsonl');
  writeFileSync(dep, jsonl([init, readEv(`${CWD}/CLAUDE.md`)]));
  writeFileSync(res, jsonl([init, readEv(`${CWD}/CLAUDE.md`), result]));
  const out = JSON.parse(execFileSync(
    'node',
    ['scripts/agent-pool.mjs', 'prefix-diff', '--deposited', dep, '--resuming', res],
    { encoding: 'utf8' }).trim());
  assert.equal(out.identical, true);
  assert.equal(out.cleanPrefix, true);
});
