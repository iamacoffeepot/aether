#!/usr/bin/env node
// Warm session pool — the pure-logic half (#3286, design of record: #3264
// design comment v1.5). Derives a pool manifest from a box's stream-json
// transcript and judges a candidate manifest's eligibility against a fresh
// checkout. All S3 and git side effects live in the workflow steps; this
// script reads files it is handed and prints one JSON object to stdout, so
// every decision is unit-testable. Exit code is 0 on any well-formed
// decision (the workflow branches on the printed `ok`), nonzero only on
// unusable input.
//
// Subcommands:
//   manifest --transcript <jsonl> --ls-tree <file> --verdict <json> \
//            [--prior <manifest.json>] [--cli-version <v>]
//     → {ok: true, manifest} | {ok: false, reason}
//     The hygiene gates (clean result, no compaction, explicit repool: yes,
//     under the context cap) are enforced here; a refusal names its reason
//     (error-exit | compacted | missing-verdict | declined | over-cap).
//
//   eligible --manifest <json> --ls-tree <file>
//     → {ok: true} | {ok: false, reason: past-cutoff | over-cap}
//     Age is judged against AGENT_POOL_CUTOFF_MINS; the cap re-check makes
//     lowering AGENT_POOL_CONTEXT_CAP_TOKENS retire existing entries
//     retroactively. Belief-truth (subtree/out-of-root blob matching) was
//     retired by #3341: a resumed session's beliefs may be stale, but every
//     fact that decides an action is re-derived on the current checkout, so
//     re-checking those beliefs at pool level was redundant and cost real
//     reuse (zero non-judge sessions ever survived it).
//
//   root
//     stdin: newline-separated repo-relative paths (a PR's changed files)
//     → {root, slug} — the request-side subsystem key for the S3 idx path.
//
//   resume-cost --transcript <jsonl>
//     → prints `write=<n> read=<n>` — the FIRST main-loop turn's cache
//     accounting (cache_creation / cache_read). The warm-resume drift signal
//     (#3347): a byte-stable resume writes tens of tokens, a drifted one
//     re-writes the whole head. Model-agnostic (reads the first usage-bearing
//     main-loop turn regardless of model — the canary probes on haiku).
//
//   prefix-diff --deposited <jsonl> --resuming <jsonl>
//     → {identical, cleanPrefix, sharedBytes, ...} when the shared region is
//       byte-identical, or {identical: false, offset, eventIndex, byteInLine,
//       event, deposited, resuming} localizing the first divergent byte.
//     The decisive resume cache-miss diagnostic (#3422). A resuming session's
//     transcript is the replayed prior turns PLUS its new turns, so the
//     deposited transcript it resumed should be a byte-identical PREFIX of it.
//     A divergence inside the shared prefix localizes the drift to the
//     deposit/replay path (in-repo); a clean prefix means the on-disk bytes are
//     stable and any miss is the CLI's request serialization (harness/API). The
//     comparison is on UTF-8 BYTES, not UTF-16 code units — the drift is
//     measured against the cached byte prefix.
//
// Knobs (env, defaults per the design): AGENT_POOL_CUTOFF_MINS=55,
// AGENT_POOL_CONTEXT_CAP_TOKENS=150000.

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

const CUTOFF_MINS = Number(process.env.AGENT_POOL_CUTOFF_MINS || 55);
const CONTEXT_CAP = Number(process.env.AGENT_POOL_CONTEXT_CAP_TOKENS || 150000);

const sha256 = (s) => createHash('sha256').update(s).digest('hex');

export function parseLsTree(text) {
  // `git ls-tree -r HEAD` lines: "<mode> blob <hash>\t<path>"
  const map = new Map();
  for (const line of text.split('\n')) {
    const tab = line.indexOf('\t');
    if (tab === -1) continue;
    const [, kind, hash] = line.slice(0, tab).split(/\s+/);
    if (kind === 'blob') map.set(line.slice(tab + 1), hash);
  }
  return map;
}

export function narrowestRoot(paths) {
  // Narrowest directory containing every path; "." for a repo-wide set.
  if (paths.length === 0) return '.';
  let parts = paths[0].split('/').slice(0, -1);
  for (const p of paths.slice(1)) {
    const q = p.split('/').slice(0, -1);
    let i = 0;
    while (i < parts.length && i < q.length && parts[i] === q[i]) i++;
    parts = parts.slice(0, i);
  }
  return parts.length ? parts.join('/') : '.';
}

export function slugify(root) {
  return root === '.' ? '_root' : root.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
}

export function subtreeHash(lsTree, root) {
  const prefix = root === '.' ? '' : root.replace(/\/$/, '') + '/';
  const lines = [...lsTree.entries()]
    .filter(([path]) => path.startsWith(prefix))
    .map(([path, hash]) => `${path}:${hash}`)
    .sort();
  return sha256(lines.join('\n'));
}

export function parseTranscript(text) {
  // Extract the pool-relevant facts: the init event, the result record, the
  // MAIN-LOOP Read set (sidechain context never enters the resumable main
  // loop, so subagent reads are correctly excluded), the terminal context
  // size, and whether the session ever compacted.
  let init = null, result = null, compacted = false, contextTokens = 0;
  let turn1CacheCreation = null, turn1CacheRead = null;
  const reads = new Set();
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let ev;
    try { ev = JSON.parse(line); } catch { continue; }
    if (ev.type === 'system' && ev.subtype === 'init') init = ev;
    else if (ev.type === 'system' && /compact/.test(ev.subtype || '')) compacted = true;
    else if (ev.type === 'result') result = ev;
    else if (ev.type === 'assistant' && !ev.parent_tool_use_id) {
      const msg = ev.message || {};
      const u = msg.usage;
      // Turn-1 resume cost (#3347): the FIRST usage-bearing main-loop turn's
      // cache accounting, captured BEFORE the haiku skip below — the canary
      // probes on a cheap haiku model with no subagents, and a real warm
      // resume's turn-1 can itself be a haiku session, so the drift signal
      // must read the first turn regardless of model.
      if (u && turn1CacheCreation === null) {
        turn1CacheCreation = u.cache_creation_input_tokens || 0;
        turn1CacheRead = u.cache_read_input_tokens || 0;
      }
      if (/haiku/.test(msg.model || '')) continue;
      if (u) {
        contextTokens = (u.cache_read_input_tokens || 0)
          + (u.cache_creation_input_tokens || 0) + (u.input_tokens || 0);
      }
      for (const blk of msg.content || []) {
        if (blk?.type === 'tool_use' && blk.name === 'Read' && blk.input?.file_path) {
          reads.add(blk.input.file_path);
        }
      }
    }
  }
  return { init, result, compacted, contextTokens, turn1CacheCreation, turn1CacheRead, reads: [...reads] };
}

export function buildManifest({ transcript, lsTree, verdict, prior, cliVersion, now }) {
  const { init, result, compacted, contextTokens, reads } = transcript;
  if (!init) return { ok: false, reason: 'no-init' };
  if (!result || result.subtype !== 'success' || result.is_error) {
    return { ok: false, reason: 'error-exit' };
  }
  if (compacted) return { ok: false, reason: 'compacted' };
  if (!verdict || verdict.repool !== 'yes') {
    return { ok: false, reason: verdict ? 'declined' : 'missing-verdict' };
  }
  if (contextTokens > CONTEXT_CAP) return { ok: false, reason: 'over-cap' };

  const cwd = init.cwd.replace(/\/$/, '');
  // A resumed run's transcript holds only the new turns; the declared set is
  // cumulative for the session's life, so merge the prior manifest's set.
  const repoReads = new Set(prior?.read_files || []);
  const external = new Set(prior?.external_reads || []);
  for (const abs of reads) {
    (abs.startsWith(cwd + '/') ? repoReads.add(abs.slice(cwd.length + 1)) : external.add(abs));
  }

  const readFiles = [...repoReads].sort();
  const root = narrowestRoot(readFiles);
  const prefix = root === '.' ? '' : root + '/';
  const outOfRoot = {};
  for (const rel of readFiles) {
    if (!rel.startsWith(prefix) && lsTree.has(rel)) outOfRoot[rel] = lsTree.get(rel);
  }

  return {
    ok: true,
    manifest: {
      schema: 1,
      session_id: init.session_id,
      model: init.model,
      cwd,
      cli_version: cliVersion || null,
      tools_fingerprint: sha256(JSON.stringify(init.tools || [])).slice(0, 16),
      read_files: readFiles,
      external_reads: [...external].sort(),
      subsystem_root: root,
      slug: slugify(root),
      subtree_hash: subtreeHash(lsTree, root),
      out_of_root: outOfRoot,
      context_tokens: contextTokens,
      verdict: { reason: verdict.reason || '', knowledge_summary: verdict.knowledge_summary || '' },
      deposited_at: Math.floor(now / 1000),
    },
  };
}

export function evaluateEligibility(manifest, lsTree, now) {
  const ageMins = (Math.floor(now / 1000) - manifest.deposited_at) / 60;
  if (ageMins >= CUTOFF_MINS) return { ok: false, reason: 'past-cutoff' };
  if (manifest.context_tokens > CONTEXT_CAP) return { ok: false, reason: 'over-cap' };
  return { ok: true };
}

export function prefixDiff(deposited, resuming) {
  // The decisive warm-resume cache-miss diagnostic (#3422). A resuming
  // session's transcript is the replayed prior turns PLUS its new turns, so the
  // deposited transcript it resumed should be a byte-identical PREFIX of it.
  // Compare on a BYTE basis (UTF-8, not UTF-16 code units — the drift is
  // measured against the cached byte prefix) and report the first byte that
  // diverges within the shared region, localized to its enclosing JSONL event.
  // A clean prefix (no divergence, the deposited transcript wholly at the
  // front) is the byte-stable hit; a divergence inside the shared region is the
  // in-repo deposit/replay drift the issue exists to localize.
  const a = Buffer.from(deposited);
  const b = Buffer.from(resuming);
  const shared = Math.min(a.length, b.length);
  let i = 0;
  while (i < shared && a[i] === b[i]) i++;
  if (i === shared) {
    return {
      identical: true,
      cleanPrefix: a.length <= b.length,
      sharedBytes: shared,
      depositedBytes: a.length,
      resumingBytes: b.length,
    };
  }
  const { eventIndex, lineStart, event } = locateEvent(a, i);
  const window = 48;
  return {
    identical: false,
    offset: i,
    eventIndex,
    byteInLine: i - lineStart,
    event,
    depositedBytes: a.length,
    resumingBytes: b.length,
    deposited: a.subarray(Math.max(0, i - window), i + window).toString('utf8'),
    resuming: b.subarray(Math.max(0, i - window), i + window).toString('utf8'),
  };
}

function locateEvent(buf, offset) {
  // Byte `offset` falls inside the JSONL line that starts after the last
  // newline before it; return that line's event index, its start offset, and
  // its parsed {type, subtype} when the line is valid JSON (the differing byte
  // can land in a partial or corrupt line, leaving event null).
  let lineStart = 0, eventIndex = 0;
  for (let j = 0; j < offset; j++) {
    if (buf[j] === 0x0a) { eventIndex++; lineStart = j + 1; }
  }
  let lineEnd = buf.indexOf(0x0a, lineStart);
  if (lineEnd === -1) lineEnd = buf.length;
  let event = null;
  try {
    const ev = JSON.parse(buf.subarray(lineStart, lineEnd).toString('utf8'));
    event = { type: ev.type ?? null, subtype: ev.subtype ?? null };
  } catch { /* the differing byte falls in a partial or corrupt line */ }
  return { eventIndex, lineStart, event };
}

function arg(name) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? undefined : process.argv[i + 1];
}

function main() {
  const cmd = process.argv[2];
  if (cmd === 'manifest') {
    const transcript = parseTranscript(readFileSync(arg('transcript'), 'utf8'));
    const lsTree = parseLsTree(readFileSync(arg('ls-tree'), 'utf8'));
    let verdict = null;
    try { verdict = JSON.parse(readFileSync(arg('verdict'), 'utf8')); } catch { /* missing-verdict */ }
    let prior = null;
    if (arg('prior')) {
      try { prior = JSON.parse(readFileSync(arg('prior'), 'utf8')); } catch { /* fresh deposit */ }
    }
    console.log(JSON.stringify(buildManifest({
      transcript, lsTree, verdict, prior, cliVersion: arg('cli-version'), now: Date.now(),
    })));
  } else if (cmd === 'eligible') {
    const manifest = JSON.parse(readFileSync(arg('manifest'), 'utf8'));
    const lsTree = parseLsTree(readFileSync(arg('ls-tree'), 'utf8'));
    console.log(JSON.stringify(evaluateEligibility(manifest, lsTree, Date.now())));
  } else if (cmd === 'root') {
    const paths = readFileSync(0, 'utf8').split('\n').map((s) => s.trim()).filter(Boolean);
    const root = narrowestRoot(paths);
    console.log(JSON.stringify({ root, slug: slugify(root) }));
  } else if (cmd === 'resume-cost') {
    const t = parseTranscript(readFileSync(arg('transcript'), 'utf8'));
    console.log(`write=${t.turn1CacheCreation ?? 0} read=${t.turn1CacheRead ?? 0}`);
  } else if (cmd === 'prefix-diff') {
    // Read the transcripts as raw buffers — the diff is byte-exact, so no
    // encoding round-trip may normalize the very bytes under examination.
    const deposited = readFileSync(arg('deposited'));
    const resuming = readFileSync(arg('resuming'));
    console.log(JSON.stringify(prefixDiff(deposited, resuming)));
  } else {
    console.error('usage: agent-pool.mjs manifest|eligible|root|resume-cost|prefix-diff [--flags]');
    process.exit(2);
  }
}

if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) main();
