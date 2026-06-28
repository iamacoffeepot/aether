export const meta = {
  name: 'audit-quality',
  description: "Audit a scoped Rust file set for code-quality characteristics that are NOT mechanically lintable (clippy/rustc/AST cannot catch them) but DO need judgment: naming, primitive overuse (Arc/Rc/clone over plain ownership), module structure, visibility, control-flow/expression shape, and type design. A six-lens judge panel under one north star — the fewest characters that still make sense — scored both ways (over-verbose AND over-terse). Returns an issue-ready rollup of shorten/clarify findings.",
  whenToUse: "When triaging a crate (or file set) for code-quality cleanups that a lint can't decide — verbose or over-clever code, Arc/Rc/clone where ownership suffices, leaked visibility, names that restate context, primitive-obsessed types, awkward control flow. Scope the file list first (the .rs files to judge) and pass absolute paths. Output is a triage rollup for human review; filing the cleanup issue is a separate /sketch step. It never gates CI and never touches GitHub.",
  phases: [
    { title: 'Classify', detail: 'one specialist agent per file x lens applies that lens taxonomy + the north-star scoring' },
    { title: 'Verify', detail: 'refute low/med-confidence flags (false positives) and challenge clean lenses (false negatives)' },
  ],
}

// audit-quality — a judge panel for the quality characteristics a lint can't own.
//
// Sibling of audit-tests.js, same skeleton (classify -> two-sided verify -> deterministic
// rollup), with two deliberate adaptations:
//   - The classify unit is (file x lens), not (file). Each of the six lenses is a SPECIALIST
//     judge: for every file, the selected lens-classifier agents run in parallel, each applying
//     only its own taxonomy. This is the "judge panel" — specialists beat one agent juggling
//     six taxonomies in a bloated prompt. Files are pipelined (no barrier between them).
//   - Every lens carries an explicit CARVE-OUT naming the clippy/rustc-settled sub-checks it
//     must skip, so the panel never re-litigates a mechanically-decided case. The judge stays
//     in clippy's allow-by-default territory (pedantic/restriction) — which is, by Clippy's own
//     architecture, exactly the subjective/context-dependent zone a deterministic gate declines.
//
// The north star is bidirectional: a finding is either OVER-VERBOSE (a strictly shorter form
// reads at least as clearly) or OVER-TERSE (the terseness costs clarity or safety — an if-let
// that drops a match's exhaustiveness, a combinator chain that reads more like Lisp than Rust).
// "Fewest characters that still make sense", not "shortest".
//
// args = {
//   files,   [string]  — ABSOLUTE paths to the .rs files to audit (REQUIRED).
//   lenses,  [string]  — optional subset of lens keys to run (default: all six). Keys:
//                        naming, primitive-overuse, module-structure, visibility,
//                        control-flow, type-design.
// }
//
// returns { rollup, files }, where rollup = { totals, confirmed, grouped, spared, uncertain,
// clippyCandidates }. confirmed is issue-ready — each row tagged by how it was confirmed
// (high-confidence | refuter | challenger-missed) and carrying its suggested_form + char_delta;
// grouped is by file then lens so it drops straight into /sketch. clippyCandidates is the
// mechanically-decidable residue the panel noticed — seed for a separate clippy.toml chore,
// NOT part of the judgment rollup.
//
// Scope the file list yourself before invoking, e.g.
//   git ls-files 'crates/<crate>/src/**/*.rs'
// Filing the cleanup issue is a human-gated step AFTER review, via /sketch — this workflow
// finds, scores, and verifies; it never touches GitHub and is never wired into preflight.

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const FILES = Array.isArray(A.files) ? A.files : []
if (!FILES.length) throw new Error('audit-quality: args.files must be a non-empty array of absolute .rs file paths')

// The six lenses. Each: key, name, taxonomy (the judgment shapes it flags, both north-star
// directions), and carveOut (the mechanically-settled sub-checks to SKIP — route any such
// observation to clippy_candidates instead of findings). Grounded in CLAUDE.md + the feedback
// memories and a deep-research pass on Rust quality that resists mechanical linting.
const ALL_LENSES = [
  {
    key: 'naming',
    name: 'Naming',
    taxonomy: `Names that cost characters without adding meaning, or cost clarity to save them.
- OVER-VERBOSE: a name that restates its enclosing context (config.config_path -> config.path; Window::window_width -> Window::width); a Rust type encoded in the name (parse_u32_millis -> parse_millis — CLAUDE.md bans u32/u64/usize in identifiers, the signature already states the type); a unit abbreviated to two letters (ms/ns/us/kb -> spell out millis/nanos/micros/bytes — CLAUDE.md, two letters is the ambiguous zone); a multi-letter generic parameter that reads as a type alias at use sites (Ctx/KindT -> single letter C/K — CLAUDE.md); filler qualifiers that add nothing (the_, _value, _data, _obj).
- OVER-TERSE: a name compressed past recognition (def_cfg_pth, n2 for a long-lived value); a single-letter binding for a non-generic value that lives across many lines.
- SEMANTIC (conversion cost, API Guidelines C-CONV): a method prefix that lies about cost — as_ must be a cheap borrow, to_ an expensive clone/compute, into_ a consuming move. Flag a to_x that merely borrows, an as_x that allocates, an into_x that does not consume self.
- HOLISTIC (C-WORD-ORDER vocabulary consistency): the same concept named two different ways across the surface (count vs len vs size for one quantity; get_/fetch_/read_ mixed for one operation).
- AETHER LAW: a passive *Capability that is actually a driver must be *DriverCapability (a FooCapability name reads passive).`,
    carveOut: `Casing (snake_case / CamelCase / SCREAMING_SNAKE) is rustc's non_snake_case / non_camel_case_types — NEVER flag casing; route any casing observation to clippy_candidates. Do not flag a name merely for being long; flag only when a strictly shorter name is at least as clear, or the current name actively misleads (wrong conversion prefix, ambiguous abbreviation).`,
  },
  {
    key: 'primitive-overuse',
    name: 'Primitive overuse',
    taxonomy: `Reaching for a heavyweight ownership / indirection primitive where a lighter one suffices.
- Arc/Rc wrapping a value that has a single owner or never crosses a thread boundary in the surrounding code -> own it (T) or borrow it (&T). The judgment is whether the sharing is REAL.
- Mutex / RwLock / RefCell / Cell / atomic inside an aether actor's state -> actor state is plain fields (ADR-0038: an actor is single-threaded behind its run-token), so interior mutability there is almost always a porting reflex, not a need.
- Box<T> indirection with no trait-object, no recursive type, and no large-move justification.
- .clone() to dodge a borrow the compiler would accept with a reference or a small restructure (clone-to-satisfy-borrowck), or a clone of a Copy-cheap value.
- OVER-TERSE counter-direction: a hand-rolled unsafe cell or a manual reference count where Rc/Arc is the honest, clearer primitive — terseness that trades away safety.
- REUSE sub-rubric: hand-rolled logic an existing primitive already owns — geometry (cross/dot/normalize/aabb) that aether-math provides; a mailbox address hand-hashed from a name where the typed ctx.actor::<Cap>() resolver belongs; a re-implemented standard container; a hand-written trait impl identical to the trait's default method.`,
    carveOut: `Genuinely shared cross-thread ownership NEEDS Arc — flag only when single-owner or single-thread is evident from the surrounding code, not on sight of the type. Clippy's redundant_clone / rc_buffer / arc_with_non_send_sync cover narrow mechanical cases; where one of those would already fire, route it to clippy_candidates. The judge does the semantic "is this sharing/indirection actually required here" call those lints cannot make.`,
  },
  {
    key: 'module-structure',
    name: 'Module structure',
    taxonomy: `Organization that obscures intent (cohesion / coupling — judgment about responsibility, not a metric).
- A god-module: one file accreting unrelated responsibilities that should split (low cohesion). Name the distinct responsibilities.
- Suffix-sibling files (foo_x.rs, foo_y.rs) -> a parent module dir foo/{mod.rs,x.rs,y.rs} (CLAUDE.md: group siblings under a parent module, always, module not crate).
- Code living in the wrong module/crate for its responsibility (high coupling): a type or fn used only by one downstream unit sitting upstream of it; a helper far from its only caller.
- Long inline ::-qualified paths repeated where a use import reads better (CLAUDE.md: pull via use, cfg-gated if needed).
- A section-divider banner comment (// ---- label ----) standing in for a real split — the fix is a module, not a comment (CLAUDE.md bans dividers).
- OVER-TERSE counter-direction: collapsing genuinely distinct concerns into one module just to save a file.`,
    carveOut: `scripts/check-no-dividers.sh already gates banner comments mechanically — if you see one, route it to clippy_candidates rather than flagging it as a judgment finding. Do not flag on file size or file count alone; flag low cohesion or wrong-home with the specific responsibility / misplaced item named.`,
  },
  {
    key: 'visibility',
    name: 'Visibility',
    taxonomy: `Public surface broader than the code needs (minimal-surface API design).
- pub on an item used only within its crate -> pub(crate); pub on an item used only within its module subtree -> private or pub(super). Judge against where it is actually referenced.
- A pub field on a struct that carries an invariant (something construction or mutation must preserve) -> private with a constructor/accessor (API Guidelines C-STRUCT-PRIVATE). The judgment is whether an invariant EXISTS, not a pub count.
- An internal/implementation type leaked through a public signature -> wrap it (C-NEWTYPE-HIDE): a public fn returning or taking a type the consumer should not depend on.
- A pub use re-export no external consumer needs.
- OVER-TERSE counter-direction: a field made private behind a getter/setter pair that just passes through with NO invariant — the accessor ceremony is the verbosity; an honest pub field is the fewest-characters form.`,
    carveOut: `rustc's unreachable_pub and dead_code already catch the truly-unused / unreachable-pub case mechanically — route those to clippy_candidates. The judge does the "this pub is reachable but broader than its use / leaks an invariant-bearing field" semantic call, which needs to see where the item is used and whether an invariant rides on it.`,
  },
  {
    key: 'control-flow',
    name: 'Control-flow & expression shape',
    taxonomy: `The sharpest north-star surface — where conciseness trades against clarity and safety.
- OVER-VERBOSE: a manual for-loop accumulating into a Vec that a map/filter/collect chain states clearer; nested if-let / match causing rightward drift that let-else flattens onto the happy path (RFC 3137); a match/if-let on Option/Result that ?, map_or, unwrap_or_else, or ok_or shortens; a redundant else-after-return or an explicit return on the tail expression.
- OVER-TERSE (the safety / clarity cost the north star must catch): an if-let or _ => () that drops a match's exhaustiveness where a future enum variant SHOULD force a compile error — flag the conciseness as a hazard (Rust Book frames this as the explicit tradeoff: if-let is less typing but loses exhaustive checking); a combinator chain so deep it "reads more like a Lisp program than Rust" where a plain loop or a named intermediate binding is clearer; a clever one-liner that hides a side effect or a non-obvious short-circuit; turbofish / let-else golfed past readability.`,
    carveOut: `Clippy owns the mechanical rewrites in its warn/style/complexity groups — single_match, needless_return, manual_map, collapsible_if, needless_range_loop, manual_let_else, etc. Where clippy would already fire on the exact rewrite, route it to clippy_candidates and do NOT re-flag it. The judge does the call clippy DECLINES: whether dropping exhaustiveness is safe at THIS site, whether a combinator chain has gone too far, whether the terse form genuinely reads clearer. (manual_let_else is allow-by-default pedantic precisely because that clarity win is debatable — the debate is the judge's job, not the lint's.)`,
  },
  {
    key: 'type-design',
    name: 'Type design',
    taxonomy: `Meaning that should live in a type rather than a bare primitive (newtype vs primitive obsession).
- Primitive obsession: a raw u32/u64/usize/String/f32 threaded through signatures where a newtype would prevent a mixup and document intent (API Guidelines C-NEWTYPE static distinctions) — e.g. two u64 ids of different kinds passed adjacently, or a bare f32 that is really Seconds vs Pixels.
- A bool parameter (especially more than one) that should be a two-variant enum so the call site reads: set_visible(true) -> set_visibility(Visibility::Visible) (C-CUSTOM-TYPE: arguments convey meaning through types, not bool/Option).
- An Option<T> argument encoding a mode that an enum would name; a stringly-typed value (&str matched against literals) that should be an enum.
- OVER-TERSE counter-direction: a newtype wrapper with NO invariant and no static-distinction value that just adds ceremony at every construction/access site — here the bare primitive is the fewest-characters honest form. The judgment is whether the distinction earns the wrapper.`,
    carveOut: `Clippy's fn_params_excessive_bools (pedantic) only COUNTS bools past a threshold and judges nothing about meaning — if a finding is purely "too many bools by count", route it to clippy_candidates. The judge decides whether a SPECIFIC bool / Option / primitive should carry meaning as a type given a plausible mixup or an unreadable call site. Do not flag a primitive merely for being primitive.`,
  },
]

const LENSES = Array.isArray(A.lenses) && A.lenses.length
  ? ALL_LENSES.filter(l => A.lenses.includes(l.key))
  : ALL_LENSES
if (!LENSES.length) throw new Error(`audit-quality: no lenses selected; valid keys are ${ALL_LENSES.map(l => l.key).join(', ')}`)

const NORTH_STAR = `THE NORTH STAR for every judgment: "the fewest characters that still make sense." A finding is one of two directions.
- OVER-VERBOSE: a strictly shorter form reads at least as clearly and is at least as correct/safe. The win is real only if the shorter form does not lose meaning, safety (e.g. exhaustiveness), or readability.
- OVER-TERSE: the current code is compressed past clarity or safety — the fix ADDS characters to restore them (an if-let that should be an exhaustive match, a Lisp-like combinator chain that should be a loop, a name too short to read).
Ceremony is not evidence. A confident-looking abstraction, a long name, or a clever one-liner is not automatically a finding — flag only when you can state the concrete better form AND why it is at least as clear/correct. When unsure, do not flag.`

const base = (f) => f.split('/').slice(-1)[0]
const short = (f) => f.split('/').slice(-2).join('/')

phase('Classify')

const CLASSIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['file', 'lens', 'findings', 'clippy_candidates'],
  properties: {
    file: { type: 'string' },
    lens: { type: 'string' },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['symbol', 'line', 'direction', 'current_form', 'suggested_form', 'char_delta', 'rationale', 'confidence'],
        properties: {
          symbol: { type: 'string', description: 'the item the finding is about — fn/struct/field/binding name and a short locator' },
          line: { type: 'integer', description: 'approximate line of the site (advisory)' },
          direction: { type: 'string', enum: ['over-verbose', 'over-terse'] },
          current_form: { type: 'string', description: 'the current code, briefly' },
          suggested_form: { type: 'string', description: 'the proposed better form' },
          char_delta: { type: 'integer', description: 'suggested length minus current length, in characters: negative = shorter suggestion, positive = the over-terse fix adds chars to restore clarity' },
          rationale: { type: 'string', description: 'why the suggested form is at least as clear/correct — the judgment, not a restatement' },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
      },
    },
    clippy_candidates: {
      type: 'array',
      description: 'observations that are mechanically decidable (this lens carve-out) and belong in clippy.toml / a lint, NOT in the judgment rollup',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['symbol', 'note'],
        properties: { symbol: { type: 'string' }, note: { type: 'string', description: 'the mechanical rule that already covers it (e.g. clippy::single_match, non_snake_case, check-no-dividers)' } },
      },
    },
  },
}

const VERIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['symbol', 'final_verdict', 'rationale'],
  properties: {
    symbol: { type: 'string' },
    final_verdict: { type: 'string', enum: ['confirmed', 'false-positive', 'uncertain'] },
    rationale: { type: 'string', description: 'why the suggested form genuinely wins under the bar, OR why it is a false positive' },
  },
}

// Challenge a CLEAN lens (one that returned no findings on a file) for false negatives — the
// shape a single classifier pass tends to miss. The challenger re-reads the file under that
// lens and may surface findings the classifier did not.
const CHALLENGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['lens', 'final_verdict', 'missed'],
  properties: {
    lens: { type: 'string' },
    final_verdict: { type: 'string', enum: ['clean-confirmed', 'missed', 'uncertain'] },
    missed: {
      type: 'array',
      description: 'findings the classifier missed (empty if clean-confirmed)',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['symbol', 'line', 'direction', 'suggested_form', 'char_delta', 'rationale'],
        properties: {
          symbol: { type: 'string' },
          line: { type: 'integer' },
          direction: { type: 'string', enum: ['over-verbose', 'over-terse'] },
          suggested_form: { type: 'string' },
          char_delta: { type: 'integer' },
          rationale: { type: 'string' },
        },
      },
    },
  },
}

function classifyPrompt(file, lens) {
  return `You are one specialist judge on a code-quality panel, auditing a single Rust file under a single lens. Read the full file at ${file} (and the types/helpers it references where you need them to judge).

Lens: ${lens.name}
${lens.taxonomy}

CARVE-OUT (mechanically settled — do NOT flag as judgment findings; route any such observation to clippy_candidates): ${lens.carveOut}

${NORTH_STAR}

Go through ${file} and report every site this lens flags, as findings. For each: the symbol + approximate line, the direction (over-verbose | over-terse), the current form, the suggested better form, char_delta (suggested length minus current length in characters), a rationale stating the judgment (why the suggested form is at least as clear and correct — not a restatement), and your confidence. Report nothing this lens does not own — another panelist covers the other lenses. If the file is clean under this lens, return an empty findings array. Be precise and conservative: a confident story is not a finding; flag only when you can name the concrete better form and why it wins.`
}

const results = await pipeline(
  FILES,
  // Stage 1 — classify: run the selected lenses for one file in parallel (the panel).
  (file) => parallel(LENSES.map(lens => () =>
    agent(classifyPrompt(file, lens), {
      label: `classify:${lens.key}:${base(file)}`,
      phase: 'Classify',
      schema: CLASSIFY_SCHEMA,
    }).then(r => ({ lens, result: r }))
  )),
  // Stage 2 — verify (two-sided). Refute low/med-confidence flags (false positives) and
  // challenge clean lenses (false negatives), under one shared strict bar. The await is why
  // this stage is async; the two sides run concurrently.
  async (lensRuns, file) => {
    const runs = (lensRuns || []).filter(Boolean).filter(r => r.result)
    if (!runs.length) return { file, runs: [], verified: [], challenged: [] }

    const BAR = `The bar for keeping a finding (identical both directions): the suggested form must be "the fewest characters that still make sense" — strictly better, not merely different.
- For OVER-VERBOSE: the shorter form must read at least as clearly AND lose no meaning, safety, or exhaustiveness. Reject it if the current verbosity is load-bearing (it documents intent, preserves a compile-time check, or aids a reader at a genuinely complex site).
- For OVER-TERSE: the current code must genuinely cost clarity or safety such that the (longer) fix is worth the characters. Reject it if the terse form is idiomatic and clear.
Reject anything that is mechanically settled (a clippy/rustc lint already owns it — that is the lens carve-out, not a judgment finding) or that rests on taste alone ("I would write it differently" is not a win). Policy anchors: CLAUDE.md conventions and the Rust API Guidelines.`

    // Refute only LOW/MEDIUM-confidence flags. A high-confidence flag on an obvious shape
    // (a name that restates its struct, an Arc on a provably single-owner value) is not a
    // judgment call and does not need an adversary — the human reviews the final list anyway.
    const flags = []
    for (const r of runs) for (const f of (r.result.findings || [])) flags.push({ ...f, lens: r.lens })
    const toRefute = flags.filter(f => f.confidence !== 'high')

    const refute = parallel(toRefute.map(f => () =>
      agent(
        `A code-quality finding was raised under the ${f.lens.name} lens. Decide whether it survives a STRICT bar — do not rescue it with a plausible story, and do not reject a real win out of conservatism.\n\nFile: ${file}\nSite: ${f.symbol} (around line ${f.line})\nDirection: ${f.direction}\nCurrent form: ${f.current_form}\nSuggested form: ${f.suggested_form}\nClassifier rationale: ${f.rationale}\n\nRead the site and the code it depends on.\n\n${BAR}\n\nIf the finding genuinely meets the bar, final_verdict='confirmed' and rationale = the concrete reason the suggested form wins. If the current code is fine (or the verbosity/terseness is load-bearing), final_verdict='false-positive'. Use 'uncertain' only when you cannot read the relevant code.`,
        { label: `refute:${f.lens.key}:${f.symbol}`.slice(0, 80), phase: 'Verify', schema: VERIFY_SCHEMA }
      ).then(v => ({ ...f, file, verify: v }))
    ))

    // Challenge each CLEAN lens (no findings on this file) for a missed issue. This is the
    // false-negative guard the refuter cannot reach, since the refuter only runs on flags.
    const cleanRuns = runs.filter(r => !(r.result.findings || []).length)
    const challenge = parallel(cleanRuns.map(r => () =>
      agent(
        `The ${r.lens.name} lens reported NO findings for this file. Your job is to CHALLENGE that clean verdict — re-read the file and look specifically for what this lens would catch that a first pass missed.\n\nFile: ${file}\n\nLens: ${r.lens.name}\n${r.lens.taxonomy}\n\nCARVE-OUT (do NOT raise these — mechanically settled): ${r.lens.carveOut}\n\n${NORTH_STAR}\n\n${BAR}\n\nIf the file is genuinely clean under this lens, final_verdict='clean-confirmed' and missed=[]. If you find real issues that meet the bar, final_verdict='missed' and list them in missed[] (symbol, line, direction, suggested_form, char_delta, rationale). Use 'uncertain' only when you cannot read the relevant code.`,
        { label: `challenge:${r.lens.key}:${base(file)}`, phase: 'Verify', schema: CHALLENGE_SCHEMA }
      ).then(c => ({ lens: r.lens, file, challenge: c }))
    ))

    const verified = (await refute).filter(Boolean)
    const challenged = (await challenge).filter(Boolean)
    return { file, runs, verified, challenged }
  }
)

// Deterministic rollup — reconcile classifier + verify + challenge into an issue-ready list.
// A finding is CONFIRMED three ways: high-confidence (accepted without a refuter), refuter
// 'confirmed' (a low/med flag it upheld), or challenger 'missed' (a finding a clean lens
// overlooked). 'spared' = a flag the refuter overturned to a false positive.
const clean = results.filter(Boolean)
const totals = { files: 0, lensRuns: 0, flags: 0, confirmed: 0, falsePositives: 0, challengerMissed: 0, charsSaveable: 0 }
const confirmed = [], spared = [], uncertain = [], clippyCandidates = []

for (const e of clean) {
  if (!e.runs || !e.runs.length) continue
  totals.files++
  const file = base(e.file)
  const verifyBySym = new Map((e.verified || []).map(v => [`${v.lens.key}:${v.symbol}`, v.verify]))

  for (const r of e.runs) {
    totals.lensRuns++
    for (const c of (r.result.clippy_candidates || [])) clippyCandidates.push({ file, lens: r.lens.key, symbol: c.symbol, note: c.note })
    for (const f of (r.result.findings || [])) {
      totals.flags++
      const row = { file, lens: r.lens.key, line: f.line, symbol: f.symbol, direction: f.direction, suggested_form: f.suggested_form, char_delta: f.char_delta }
      if (f.confidence === 'high') {
        confirmed.push({ ...row, source: 'high-confidence' })
      } else {
        const v = verifyBySym.get(`${r.lens.key}:${f.symbol}`)
        if (v?.final_verdict === 'confirmed') confirmed.push({ ...row, source: 'refuter' })
        else if (v?.final_verdict === 'false-positive') { spared.push({ ...row, reason: v.rationale }); totals.falsePositives++ }
        else uncertain.push({ ...row, stage: 'refute', note: v?.rationale || 'no verify result' })
      }
    }
  }

  // Challenger-surfaced misses from clean lenses.
  for (const ch of (e.challenged || [])) {
    if (ch.challenge?.final_verdict === 'missed') {
      for (const m of (ch.challenge.missed || [])) {
        totals.challengerMissed++
        confirmed.push({ file, lens: ch.lens.key, line: m.line, symbol: m.symbol, direction: m.direction, suggested_form: m.suggested_form, char_delta: m.char_delta, source: 'challenger-missed' })
      }
    } else if (ch.challenge?.final_verdict === 'uncertain') {
      uncertain.push({ file, lens: ch.lens.key, symbol: '(clean-lens challenge)', stage: 'challenge', note: 'challenger could not read the relevant code' })
    }
  }
}

totals.confirmed = confirmed.length
totals.charsSaveable = confirmed.reduce((a, r) => a + (r.char_delta < 0 ? -r.char_delta : 0), 0)

// grouped: by file then lens, so the rollup drops straight into /sketch.
const grouped = {}
for (const r of confirmed) ((grouped[r.file] ||= {})[r.lens] ||= []).push(r)
const bySource = confirmed.reduce((a, r) => ((a[r.source] = (a[r.source] || 0) + 1), a), {})

log(`audit-quality: ${totals.files} files x ${LENSES.length} lenses -> ${totals.confirmed} confirmed findings (high-conf ${bySource['high-confidence'] || 0}, refuter ${bySource['refuter'] || 0}, challenger-missed ${bySource['challenger-missed'] || 0}), ${totals.charsSaveable} chars saveable, ${spared.length} spared, ${uncertain.length} uncertain, ${clippyCandidates.length} clippy candidates`)

return { rollup: { totals, confirmed, grouped, spared, uncertain, clippyCandidates }, files: clean }
