export const meta = {
  name: 'wish-deep',
  description: 'Deep mode for /wish — best-first fan-out drilling. Each wish node is drilled by its own fresh-context agent that writes its wish.md, returns a bounded self-summary, and is gated by an adversarial skeptic before it counts terminal. The agent boundary supplies the context erasure; per-node summaries supply the lineage hand-down.',
  whenToUse: 'Invoked by the /wish skill on `/wish --deep`. The skill runs the adversity + root-generation front of the pass inline, then hands roots + theme/role/beam/budget + an absolute wishDir to this workflow as args. Not invoked directly by the user — go through /wish --deep.',
  phases: [
    { title: 'Drill', detail: 'Best-first frontier: each round pops the top --beam scored nodes, drills them in parallel (each agent writes its own wish.md), gates producible nodes through an adversarial skeptic, scores the children, pushes them back.' },
    { title: 'Synthesize', detail: 'One agent reads every node summary and writes index.md with the cross-branch coherence the fan-out spends.' },
  ],
}

// wish-deep: best-first fan-out drilling for /wish --deep.
//
// The orchestrator (this script) holds only a lightweight scored frontier in JS.
// It has NO filesystem access — every wish.md and the final index.md is written
// by the agents themselves. That division IS the "erasure" the design asks for:
// a driller's heavy reasoning dies with its context and never reaches this loop,
// which sees only a bounded self-summary per node.
//
// args = {
//   theme,           string  — the wish theme
//   role,            string|null — optional role lens ("player", "designer", ...)
//   beam,            number  — fan-out width per round (default 3); the depth-vs-breadth knob
//   budget,          number  — max driller agents the loop spawns (default 40); the size knob
//   roots,           [{ slug, wish, doors_opened, unresolvedness }]  — generated inline by the skill
//   wishDir,         string  — ABSOLUTE path to wishes/<date>-<theme>/ (agents write here)
//   groundingNotes,  string  — shared grep-confirmed engine surfaces from the skill's step-1 scan
// }
//
// NOTE on the name `budget`: the harness injects a per-turn TOKEN budget as a
// global named `budget` (with `.total` / `.remaining()`). args.budget is a
// DIFFERENT thing — a driller-COUNT cap. We read it into `drillBudget` so the
// local never shadows the harness global, then keep the token budget underneath
// as a backstop ceiling so a large --budget can't outrun a tighter turn directive.

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const theme = A.theme || ''
const role = A.role || null
const beam = Math.max(1, Number.isFinite(A.beam) ? A.beam : 3)
const drillBudget = Math.max(1, Number.isFinite(A.budget) ? A.budget : 40)
const wishDir = A.wishDir || ''
const groundingNotes = A.groundingNotes || ''
const roots = Array.isArray(A.roots) ? A.roots : []

if (!theme || !wishDir || roots.length === 0) {
  return { error: 'wish-deep needs args = { theme, wishDir, roots:[...] }. Call it from /wish --deep, which runs the adversity + root-generation front of the pass inline and hands the roots over.' }
}

// ─── Schemas (the load-bearing driller / skeptic contract) ───

// A driller writes its own wish.md and returns this compact struct. `summary` is
// the bounded lineage hand-down its children inherit. `grounded_surfaces` is the
// grep-confirmed engine surfaces it leaned on (real code, not recalled). When
// `producible` is true the wish IS a plan and `children` should be empty.
const DRILL_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['producible', 'summary', 'grounded_surfaces', 'children'],
  properties: {
    producible: { type: 'boolean', description: 'true means this wish is now a plan — writable with known, grep-confirmed means within current resources' },
    summary: { type: 'string', description: 'bounded self-summary (2-4 sentences): the shape this wish settled on and why, enough for a child driller to compose upward without re-reading the full wish.md' },
    grounded_surfaces: { type: 'array', items: { type: 'string' }, description: 'engine surfaces this wish builds on, each grep-confirmed and cited crate/path or file:line' },
    children: {
      type: 'array',
      description: 'the absences — each becomes a sub-wish. EMPTY when producible:true.',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['slug', 'wish', 'doors_opened', 'unresolvedness'],
        properties: {
          slug: { type: 'string', description: 'lowercase kebab-case, descriptive, 20-50 chars' },
          wish: { type: 'string', description: 'the sub-wish, phrased "I wish I could X so that I could Y."' },
          doors_opened: { type: 'number', description: 'leverage 1-5: how much downstream this child unlocks if resolved' },
          unresolvedness: { type: 'number', description: 'how far from producible 1-5: 5 = a deep unknown, 1 = nearly a plan already' },
        },
      },
    },
  },
}

// A skeptic runs ONLY on a producible:true claim. It must FAIL to find a hidden
// unknown before the node counts terminal. A found unknown re-enters the frontier
// as a child and the node keeps drilling — this is what kills "good enough".
const SKEPTIC_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['hidden_unknown_found', 'unknown', 'rationale'],
  properties: {
    hidden_unknown_found: { type: 'boolean', description: 'true if the producibility claim hides an absence the driller did not drill' },
    unknown: {
      type: ['object', 'null'],
      additionalProperties: false,
      required: ['slug', 'wish', 'doors_opened', 'unresolvedness'],
      properties: {
        slug: { type: 'string' },
        wish: { type: 'string' },
        doors_opened: { type: 'number' },
        unresolvedness: { type: 'number' },
      },
      description: 'the hidden unknown as a sub-wish, or null when none found',
    },
    rationale: { type: 'string', description: 'why the claim holds (no unknown) or the specific surface/assumption that is not actually producible' },
  },
}

// ─── Prompts ───

const WISH_MD_SHAPE =
  'Write the wish.md with minimal frontmatter then free-form prose, NO internal ## headers:\n' +
  '```\n' +
  '---\n' +
  'wish: I wish I could <X> so that I could <Y>.\n' +
  'adversity: data | empathy\n' +
  'parent: ../wish.md            # omit if this node is a root\n' +
  'producible: true | false      # true means this wish IS a plan\n' +
  '---\n\n' +
  '<prose body, no headers: the wish + the adversity that grounds it + the goal it serves;\n' +
  ' the shape that would satisfy it at THIS level of depth (coarse near the root, fine near leaves);\n' +
  ' whether that shape is producible with known, grep-confirmed means within current resources;\n' +
  ' if producible: the plan, concrete enough that someone could start;\n' +
  ' if not: the absences, each named with the sub-wish that would resolve it;\n' +
  ' coherence with the parent: how this wish resolves upward into the parent plan;\n' +
  ' then the design-space context — alternatives considered (one-line shape + one-line path cost each),\n' +
  ' doors opened (what this unlocks downstream), doors closed (what it commits to / forecloses).>\n' +
  '```'

const GROUND_DISCIPLINE =
  'GROUNDING DISCIPLINE (load-bearing): every concrete engine surface this wish claims already exists ' +
  '(kind / cap / mailbox / trait / file path / signature) must be grep-confirmed against current code in ' +
  'crates/aether-*/src/ BEFORE you write it in. Cite crate/path or file:line for each. Prefer code over ' +
  'CLAUDE.md / ADRs — docs drift, the crates are authoritative. The NOVEL wished thing is invented (that is ' +
  'the point of a wish); the existing means it composes from are VERIFIED, never recalled. If you cannot ' +
  'verify a surface you were about to lean on, that is signal: either it does not exist (a deeper absence — ' +
  'make it a child) or it is named differently (find the real name). Never paper over with a plausible guess.'

const drillPrompt = (node, file) => {
  const lineage = node.ancestors.length
    ? node.ancestors.map(a => '- [' + a.slug + '] ' + a.summary).join('\n')
    : '(this is a root wish — no ancestors)'
  return (
    '## Wish driller — one node of a /wish --deep tree\n\n' +
    'Theme: ' + theme + (role ? ' (as ' + role + ')' : '') + '\n\n' +
    'Your wish: ' + node.wish + '\n' +
    'Slug: ' + node.slug + '  ·  depth: ' + node.slugChain.length + '\n\n' +
    '## Ancestor summary chain (your lineage hand-down — compose upward into these)\n' +
    lineage + '\n\n' +
    '## Shared grounding from the pass front (grep-confirmed surfaces; extend, do not re-derive)\n' +
    (groundingNotes || '(none provided)') + '\n\n' +
    '## Task\n' +
    '1. Articulate the shape that would satisfy THIS wish at its depth (coarse near the root, fine deeper down).\n' +
    '2. ' + GROUND_DISCIPLINE + '\n' +
    '3. Producibility check: can this shape be written with known, verified means within current resources ' +
    '(one engineer + Claude, modest API budget, no GPU cluster)? If yes, this wish IS a plan (producible:true, children empty). ' +
    'If no, name the absences — each becomes a child sub-wish with a doors_opened (leverage 1-5) and unresolvedness (distance-from-plan 1-5) estimate.\n' +
    '4. Write your full wish.md to the EXACT path:\n   ' + file + '\n' +
    '   ' + WISH_MD_SHAPE + '\n' +
    '5. Return the compact struct: producible, a bounded 2-4 sentence summary (your children inherit it as lineage — do not dump your whole reasoning), ' +
    'grounded_surfaces (the cited real surfaces), and children (empty if producible).\n\n' +
    'Be honest about depth: do not pad a shallow chain to look deep, do not truncate a deep one to "good enough". ' +
    'The chain stops only when producibility says so.\n\nStructured output only — and you MUST write the wish.md file.'
  )
}

const skepticPrompt = (node, res) =>
  '## Adversarial terminality skeptic\n\n' +
  'A driller claims this wish is now PRODUCIBLE — that it can be written with known, verified means. ' +
  'Your job is to FALSIFY that: find ONE hidden unknown the driller glossed — a surface it assumed exists ' +
  'without grep-confirming it, a step that is not actually writable within current resources, an integration ' +
  'seam left implicit, or a "known mean" that is really another wish. Default to finding a gap if uncertain.\n\n' +
  'Theme: ' + theme + (role ? ' (as ' + role + ')' : '') + '\n' +
  'Wish: ' + node.wish + '\n' +
  'Driller summary: ' + res.summary + '\n' +
  'Claimed grounded surfaces: ' + (res.grounded_surfaces || []).join('; ') + '\n\n' +
  'Verify the claimed surfaces against current code in crates/aether-*/src/ (grep/read — you have read-only tools). ' +
  'If you find a hidden unknown, return hidden_unknown_found:true and the unknown as a sub-wish ' +
  '(slug, wish phrased "I wish ... so that ...", doors_opened 1-5, unresolvedness 1-5). ' +
  'If the producibility claim genuinely holds — every leaned-on surface checks out and nothing is left implicit — ' +
  'return hidden_unknown_found:false and unknown:null with a rationale naming what you verified.\n\nStructured output only.'

const synthPrompt = (indexPath, nodeBlock, stats) =>
  '## Wish-tree synthesis — write the index\n\n' +
  'A /wish --deep pass drilled this tree node by node; each node below is one bounded summary (the fan-out spent ' +
  'the cross-branch coherence, your job is to recover it). Theme: ' + theme + (role ? ' (as ' + role + ')' : '') + '.\n\n' +
  '## Nodes (path · producible · wish · summary)\n' + nodeBlock + '\n\n' +
  '## Stats\n' +
  'roots: ' + stats.rootCount + '  ·  drilled nodes: ' + stats.totalNodes + '  ·  plans (leaves): ' + stats.leafCount +
  '  ·  max depth: ' + stats.maxDepth + '  ·  skeptic demotions: ' + stats.skepticDemotions +
  '  ·  named-but-undrilled (budget-bounded): ' + stats.undrilled + '\n\n' +
  '## Task\nWrite index.md to the EXACT path:\n  ' + indexPath + '\n' +
  'It is the navigation surface, not a duplicate of the wish bodies. Include: date, theme, role; the list of root ' +
  'wishes with one-line summaries; the deep-spine map (which high-leverage branches drilled deep vs which stayed ' +
  'stubs, and WHY — the cross-branch coherence); the skeptic-demoted nodes (where "good enough" was caught); the ' +
  'stats above; and a short notes paragraph. Then STOP — return a one-line confirmation that index.md was written.\n\n' +
  'You MUST write the index.md file.'

// ─── Phase: Drill (best-first frontier) ───
phase('Drill')

// Frontier node: { slug, wish, doors_opened, unresolvedness, score, slugChain, ancestors:[{slug,summary}] }
let frontier = roots.map(r => ({
  slug: r.slug,
  wish: r.wish,
  doors_opened: r.doors_opened,
  unresolvedness: r.unresolvedness,
  score: (r.doors_opened || 1) * (r.unresolvedness || 1),
  slugChain: [r.slug],
  ancestors: [],
}))

const summaries = []   // { slug, slugChain, wish, summary, producible }
let drills = 0
let maxDepth = 0
let leafCount = 0
let skepticDemotions = 0

// Backstop: the harness per-turn TOKEN budget (global `budget`) is a ceiling
// UNDER drillBudget, so a large --budget cannot outrun a tighter "+N" directive.
// Guarded because the global may be absent in some runtimes.
const tokenExhausted = () =>
  typeof budget !== 'undefined' && budget && typeof budget.remaining === 'function' &&
  budget.total && budget.remaining() <= 0

// Drill one node: write its wish.md, then gate a producible claim through the skeptic.
// Returns the data; the main loop (not this fn) mutates the shared frontier, so
// parallel drillers never race on it.
const drillNode = async (node) => {
  const file = wishDir + '/' + node.slugChain.join('/') + '/wish.md'
  const res = await agent(drillPrompt(node, file), { label: 'drill:' + node.slug, phase: 'Drill', schema: DRILL_SCHEMA })
  if (!res) return null
  let producible = !!res.producible
  const children = Array.isArray(res.children) ? res.children.slice() : []
  let demoted = false
  if (producible) {
    const sk = await agent(skepticPrompt(node, res), { label: 'skeptic:' + node.slug, phase: 'Drill', schema: SKEPTIC_SCHEMA, agentType: 'Explore' })
    if (sk && sk.hidden_unknown_found && sk.unknown) {
      producible = false   // demoted — the node keeps drilling
      demoted = true
      children.push(sk.unknown)
      log('skeptic demoted [' + node.slug + ']: ' + sk.unknown.slug)
    }
  }
  return { node, res, producible, children, demoted }
}

while (frontier.length > 0 && drills < drillBudget) {
  if (tokenExhausted()) { log('token budget backstop hit — stopping drill at ' + drills + ' drills'); break }
  // best-first: highest score first, root-proximity (shallower depth) as the tie-break
  frontier.sort((a, b) => (b.score - a.score) || (a.slugChain.length - b.slugChain.length))
  const take = Math.min(beam, drillBudget - drills, frontier.length)
  const round = frontier.splice(0, take)
  const drilled = await parallel(round.map(node => () => drillNode(node)))
  drills += round.length

  for (const out of drilled) {
    if (!out) continue
    const { node, res, producible, children, demoted } = out
    const depth = node.slugChain.length
    if (depth > maxDepth) maxDepth = depth
    if (demoted) skepticDemotions++
    summaries.push({ slug: node.slug, slugChain: node.slugChain.slice(), wish: node.wish, summary: res.summary, producible })
    if (producible && children.length === 0) {
      leafCount++
      continue
    }
    // push children back onto the frontier, scored doors_opened × unresolvedness
    const childAncestors = node.ancestors.concat([{ slug: node.slug, summary: res.summary }])
    for (const c of children) {
      frontier.push({
        slug: c.slug,
        wish: c.wish,
        doors_opened: c.doors_opened,
        unresolvedness: c.unresolvedness,
        score: (c.doors_opened || 1) * (c.unresolvedness || 1),
        slugChain: node.slugChain.concat([c.slug]),
        ancestors: childAncestors,
      })
    }
  }
}

const stats = {
  rootCount: roots.length,
  totalNodes: summaries.length,
  leafCount,
  maxDepth,
  skepticDemotions,
  drills,
  undrilled: frontier.length,   // named-but-not-materialized (budget-bounded stubs)
}
log('Drill done: ' + stats.totalNodes + ' nodes drilled, ' + stats.leafCount + ' plans, max depth ' +
  stats.maxDepth + ', ' + stats.skepticDemotions + ' skeptic demotions, ' + stats.undrilled + ' frontier left')

if (summaries.length === 0) {
  return { ...stats, error: 'No nodes drilled — every root driller returned null. Nothing written.' }
}

// ─── Phase: Synthesize ───
phase('Synthesize')
const nodeBlock = summaries
  .slice()
  .sort((a, b) => a.slugChain.join('/').localeCompare(b.slugChain.join('/')))
  .map(s => '- [' + s.slugChain.join('/') + '] (' + (s.producible ? 'plan' : 'wish') + ') ' + s.wish + '\n    ' + s.summary)
  .join('\n')

const indexPath = wishDir + '/index.md'
const synth = await agent(synthPrompt(indexPath, nodeBlock, stats), { label: 'synthesize', phase: 'Synthesize' })

return {
  ...stats,
  wishDir,
  indexWritten: !!synth,
  // surfaced to the skill for its report
  roots: roots.map(r => r.slug),
}
