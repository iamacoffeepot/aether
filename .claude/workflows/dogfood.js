export const meta = {
  name: 'dogfood',
  description: "Consumer-viewpoint validation of a landed feature: a fresh agent that never sees the implementation is handed a realistic task that exercises the new surface, accomplishes it through the public API only, and is graded on the friction it hit (the log IS the signal) plus use-visible correctness (a vision judge over the rendered artifact). The complement to the `review` workflow — review audits the producer's artifact, dogfood is a consumer-use trial that catches what only use reveals: ergonomic friction, missing primitives, awkward composition, surprising defaults, doc gaps. Three media by what the consumer must write: drive (nothing — drive the running engine over MCP), author (a guest wasm component against the SDK), build-layer (a new native cap / kind family / infra API on the workspace crates). Returns an issue-ready rollup; never touches GitHub, never gates CI (advisory + soft-hold).",
  whenToUse: "After a feature lands (or at the end of /implement, before un-draft) to trial it from the consumer's side. The caller resolves and passes the issue text, the landed diff (read by Author only, never the Attempt), and surface pointers — the workflow sandbox cannot run git/grep itself. The live MCP harness (tunnel -> hub -> fleet, scripts/ensure-tunnel.sh) must be up for the drive / render media. Output is a triage rollup for human review; filing papercut / missing-primitive / doc-gap follow-ups is a separate /sketch step.",
  phases: [
    { title: 'Author', detail: 'one agent reads the issue + diff + surface and writes a realistic "build Y that consumes X" task, picking the medium' },
    { title: 'Attempt', detail: 'a fresh agent (never the diff) accomplishes the task through the public surface, logging friction at every wall' },
    { title: 'Judge', detail: 'a vision judge re-captures the still-alive engine and grades use-visible correctness (render artifact only)' },
  ],
}

// dogfood — the consumer's-viewpoint complement to review.js.
//
// args = {
//   issue,        string  — the landed feature's issue / scope text (REQUIRED unless task is given).
//   diff,         string  — the landed diff. Read by Author ONLY to pick the task; NEVER forwarded to
//                           the Attempt (the freshness boundary — the consumer rediscovers the surface).
//   surface,      string  — pointers to the public surface under test (guide paths, crate, mail kinds,
//                           MCP tools) the Author frames the task around.
//   task,         object  — a pre-supplied / approved task (skips Author). Shape = TASK_SCHEMA. Passing
//                           this is how the human gate's second call resumes a heavy-medium run.
//   medium,       string  — force the medium (drive|author|build-layer); else the Author picks.
//   authorModel,  string  — model for the Author phase (default 'sonnet').
//   attemptModel, string  — model for the Attempt (default 'opus' — the consumer does real engineering).
//   judgeModel,   string  — model for the vision judge (default 'opus').
// }
//
// returns { proposedTask?, needsApproval?, rollup, task }. For a heavy medium (author / build-layer)
// authored fresh, the run STOPS after Author and returns { proposedTask, needsApproval:true, rollup:null }
// — a workflow cannot block on human input, so the caller reviews the task and re-invokes with args.task.
// A drive task runs straight through. rollup = { totals, succeeded, buildGreen, artifact, friction
// (grouped papercut|missing-primitive|doc-gap|blocker), softHolds }. softHolds = a wrong artifact verdict
// or any high-severity blocker — the subset a reviewer clears. friction feeds the flywheel: papercut ->
// /sketch, missing-primitive -> a build-machinery issue, doc-gap -> a guide edit.

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const AUTHOR_MODEL = A.authorModel || 'sonnet'
const ATTEMPT_MODEL = A.attemptModel || 'opus'
const JUDGE_MODEL = A.judgeModel || 'opus'

const MEDIA = ['drive', 'author', 'build-layer']
// Media whose generated task is expensive to attempt (a scratch crate + compile loop), so a fresh
// author's task is human-gated before the heavy run. drive is cheap enough to run straight through.
const HEAVY = new Set(['author', 'build-layer'])

if (!A.task && !A.issue) throw new Error('dogfood: args.issue (the landed feature text) is required unless args.task is supplied')
if (A.medium && !MEDIA.includes(A.medium)) throw new Error(`dogfood: args.medium must be one of ${MEDIA.join(', ')}`)

const TASK_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['medium', 'prompt', 'surfaceUnderTest', 'expectedArtifact'],
  properties: {
    medium: { type: 'string', enum: MEDIA, description: 'drive (drive the running engine over MCP, write no code), author (write a guest wasm component against aether-actor), build-layer (build a new native cap / kind family / infra API on the workspace crates)' },
    prompt: { type: 'string', description: 'the realistic "build Y that necessarily consumes X" task handed verbatim to the fresh Attempt agent — concrete, accomplishable, and impossible to do without touching the surface under test' },
    surfaceUnderTest: { type: 'string', description: 'the public surface this task grades — the mail kinds / MCP tools / SDK macros / infra API the consumer must lean on' },
    expectedArtifact: { type: ['string', 'null'], description: 'for a task whose result RENDERS: what the captured frame should show, in enough detail for a vision judge to rule correct/wrong (e.g. "a single cube orbiting a fixed point, visibly rotating between frames"). null when there is no visual artifact to judge.' },
  },
}

const FINDING_ITEM = {
  type: 'object',
  additionalProperties: false,
  required: ['category', 'severity', 'where', 'what', 'suggested'],
  properties: {
    category: { type: 'string', enum: ['papercut', 'missing-primitive', 'doc-gap', 'blocker'], description: 'papercut = ergonomic friction (awkward composition, surprising default, boilerplate); missing-primitive = reached for something the engine lacks (a build-machinery candidate); doc-gap = could not do it from docs/guide + public signatures and had to read crate internals; blocker = a wall that stopped the task' },
    severity: { type: 'string', enum: ['high', 'medium', 'low'] },
    where: { type: 'string', description: 'the step / surface where the friction hit (the mail kind, the macro, the API, the doc page)' },
    what: { type: 'string', description: 'the friction itself — what was awkward, missing, undocumented, or blocking, concretely' },
    suggested: { type: 'string', description: 'the consumer-side fix: a better default / signature, the missing primitive to build, the doc line to add — or "" if none obvious' },
  },
}

const FRICTION_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['succeeded', 'summary', 'engineId', 'buildGreen', 'findings'],
  properties: {
    succeeded: { type: 'boolean', description: 'did you accomplish the task through the public surface' },
    summary: { type: 'string', description: 'what you did and how it went, briefly' },
    engineId: { type: ['string', 'null'], description: 'for a render task: the engine_id you left ALIVE for the judge to capture; null otherwise (and terminate any engine you spawned)' },
    buildGreen: { type: ['boolean', 'null'], description: 'author / build-layer: did the scratch crate build (and any tests pass); null for drive' },
    findings: { type: 'array', items: FINDING_ITEM },
  },
}

const JUDGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['verdict', 'rationale'],
  properties: {
    verdict: { type: 'string', enum: ['correct', 'wrong', 'n-a'], description: 'correct = the captured frame matches the expected artifact; wrong = it does not (use-visible defect); n-a = nothing renderable to judge' },
    rationale: { type: 'string', description: 'what the frame showed vs what was expected — the concrete visual discrepancy for a wrong verdict' },
  },
}

function authorPrompt(issue, diff, surface, forcedMedium) {
  const mediumLine = forcedMedium
    ? `The medium is FIXED to "${forcedMedium}" — frame the task for it.`
    : `Pick the medium by what consumes the surface: drive (the surface is mail kinds / MCP tools / runtime behavior — drive the running engine, write no code), author (the surface is the guest SDK — write a wasm component against aether-actor), build-layer (the surface is an infra crate / capability trait / kind vocabulary — build a new native cap or kind family on top of it).`
  return `You are authoring a DOGFOOD task: a realistic job that a fresh consumer agent — one who will NOT see the implementation — must accomplish using ONLY the public surface a landed feature added. The task grades the surface by how it feels to consume.

THE LANDED FEATURE (issue / scope):
${issue}

THE DIFF (so YOU understand what shipped — the consumer will NEVER see this):
${diff || '(no diff supplied — frame the task from the issue + surface)'}

PUBLIC SURFACE UNDER TEST:
${surface || '(infer from the issue)'}

${mediumLine}

Write a task of the shape "build Y that necessarily consumes X": Y is a small but real thing a consumer would actually make, and it is impossible to finish Y without leaning on the new surface X. Make it concrete and accomplishable in one focused sitting. Do NOT leak implementation details, file paths from the diff, or the producer's framing — the consumer must rediscover the surface from the public docs as a real user would.

If the result RENDERS (a frame can be captured), set expectedArtifact to what that frame should show, specific enough for a vision judge to rule correct vs wrong. Otherwise set expectedArtifact to null.

Return medium, prompt (the task text handed verbatim to the consumer), surfaceUnderTest, expectedArtifact.`
}

function attemptPrompt(task) {
  const heavy = HEAVY.has(task.medium)
  const mediumGuide = {
    'drive': `Drive the running engine over the MCP harness (ToolSearch for the mcp__aether-hub__* tools: spawn_substrate, load_component, send_mail, send_mail_traced, capture_frame, describe_kinds, describe_component). Write no crate.`,
    'author': `Write a guest wasm component: scaffold a small cdylib crate depending on aether-actor, implement the Actor with #[actor]/#[handler], derive its kinds, export! it, build to wasm32, then load + drive it over MCP (ToolSearch for mcp__aether-hub__*).`,
    'build-layer': `Extend the engine: build a small new native capability / kind family / infra API in a scratch crate that path-depends on the workspace crates it must consume. Compile it; exercise it over MCP if it is loadable.`,
  }[task.medium]
  return `You are a FRESH consumer of the aether engine. You have NEVER seen the implementation of the surface you are about to use — discover it from the public docs as a real user would. Your job is to accomplish a task AND to honestly log every point of friction, because the friction log is the whole signal.

YOUR TASK:
${task.prompt}

SURFACE YOU MUST USE: ${task.surfaceUnderTest}
MEDIUM: ${task.medium} — ${mediumGuide}

THE FRESHNESS RULE (load-bearing):
- Work from docs/guide/ (start at docs/guide/SUMMARY.md) and public crate signatures ONLY.
- If you cannot figure out how to use the surface from the public docs + signatures and have to read crate INTERNALS (private modules, test code, the impl bodies) to proceed — that is itself a DOC-GAP finding. Log it (category doc-gap, where = what you were trying to do, what = the doc that was missing), then proceed.
- Do NOT let yourself be coached past a rough edge by reading the implementation. A real consumer cannot.

THE STALL RULE:
- Do NOT heroically work around friction — that hides the papercut. When you hit a wall (an awkward composition, a surprising default, a missing primitive, a confusing error), LOG it as a finding and either route around it minimally or, if it truly blocks the task, log a blocker finding and STOP. A real consumer's stall IS the data.
- Reach for a primitive the engine lacks? That is a missing-primitive finding (suggest what to build) — do not hand-roll it silently.

${heavy ? `BUILD: the scratch crate is yours to create anywhere under a temp/scratch dir. Build it (cargo build for build-layer; the wasm32 target for an author component). Report buildGreen = whether it compiled (and any tests passed).` : `Report buildGreen = null (you write no crate).`}

${task.expectedArtifact ? `RENDER ARTIFACT: this task renders. After you produce the visual state, spawn_substrate / drive it so the expected frame is showing, and LEAVE THE ENGINE ALIVE — report its engine_id so a judge can capture it. Do NOT terminate that engine.` : `No render artifact: terminate_substrate any engine you spawned before returning; report engineId = null.`}

Return succeeded, summary, engineId, buildGreen, and findings (every friction point — category, severity, where, what, suggested). An empty findings array means the surface was friction-free; be honest, not generous.`
}

function judgePrompt(task, engineId) {
  return `You are a vision judge for a dogfood trial. A consumer agent drove an engine to produce a visual result and left it alive for you. Capture it yourself and rule whether it is use-visibly correct.

EXPECTED ARTIFACT (what the frame should show):
${task.expectedArtifact}

Call capture_frame on engine_id "${engineId}" (ToolSearch for mcp__aether-hub__capture_frame). The PNG returns inline — look at it. Compare what you SEE against the expected artifact above. Rule:
- verdict 'correct' if the frame matches the expectation.
- verdict 'wrong' if it does not — name the concrete visual discrepancy (this is a use-visible defect the producer's tests missed).
- verdict 'n-a' only if nothing renderable came back.

After you have judged, terminate_substrate engine_id "${engineId}" to free the fleet.

Return verdict and rationale.`
}

// Phase 1 — Author (skipped when a task is supplied). A freshly-authored heavy-medium task is
// human-gated: the run returns the proposal and stops, because a workflow cannot block on input.
let task = A.task || null
if (!task) {
  phase('Author')
  task = await agent(authorPrompt(A.issue, A.diff, A.surface, A.medium), {
    label: 'author', phase: 'Author', model: AUTHOR_MODEL, schema: TASK_SCHEMA,
  })
  if (!task) throw new Error('dogfood: the Author phase produced no task')
  if (HEAVY.has(task.medium)) {
    log(`dogfood: authored a ${task.medium} task — returning for approval. Re-invoke with args.task set to the (edited) task to run the Attempt.`)
    return { proposedTask: task, needsApproval: true, rollup: null, task }
  }
  log(`dogfood: authored a ${task.medium} task — running straight through (cheap medium).`)
}

// Phase 2 — Attempt. A fresh agent, never handed the diff, accomplishes the task through the public
// surface and logs friction. Heavy media compile a scratch crate, so they get an isolated worktree.
phase('Attempt')
const attempt = await agent(attemptPrompt(task), {
  label: `attempt:${task.medium}`, phase: 'Attempt', model: ATTEMPT_MODEL,
  agentType: 'general-purpose',
  isolation: HEAVY.has(task.medium) ? 'worktree' : undefined,
  schema: FRICTION_SCHEMA,
})
if (!attempt) throw new Error('dogfood: the Attempt agent died with no friction report')

// Phase 3 — Judge (render artifact only). The judge re-captures the still-alive engine itself, so the
// PNG lands in its own vision context — no file handoff, no agent grading its own screenshot. It
// terminates the engine when done.
let judge = null
if (task.expectedArtifact && attempt.engineId) {
  phase('Judge')
  judge = await agent(judgePrompt(task, attempt.engineId), {
    label: 'judge', phase: 'Judge', model: JUDGE_MODEL, agentType: 'general-purpose', schema: JUDGE_SCHEMA,
  })
} else if (task.expectedArtifact && !attempt.engineId) {
  log('dogfood: task expected a render artifact but the Attempt left no live engine — artifact unjudged.')
}

// Deterministic rollup. Friction is grouped by category; soft-holds are a wrong artifact verdict or any
// high-severity blocker — the subset a reviewer must clear before trusting the surface.
const findings = (attempt.findings || [])
const byCategory = { papercut: [], 'missing-primitive': [], 'doc-gap': [], blocker: [] }
for (const f of findings) (byCategory[f.category] ||= []).push(f)

const softHolds = []
if (judge && judge.verdict === 'wrong') softHolds.push({ kind: 'use-visible-incorrect', detail: judge.rationale })
for (const f of findings) if (f.category === 'blocker' && f.severity === 'high') softHolds.push({ kind: 'blocker', where: f.where, detail: f.what })

const totals = {
  findings: findings.length,
  papercut: byCategory.papercut.length,
  missingPrimitive: byCategory['missing-primitive'].length,
  docGap: byCategory['doc-gap'].length,
  blocker: byCategory.blocker.length,
  softHolds: softHolds.length,
}

log(`dogfood [${task.medium}]: succeeded=${attempt.succeeded}${attempt.buildGreen === null ? '' : ` buildGreen=${attempt.buildGreen}`}${judge ? ` artifact=${judge.verdict}` : ''} — ${totals.findings} findings (papercut ${totals.papercut}, missing-primitive ${totals.missingPrimitive}, doc-gap ${totals.docGap}, blocker ${totals.blocker}), ${softHolds.length} SOFT-HOLD`)

return {
  rollup: {
    totals,
    succeeded: attempt.succeeded,
    buildGreen: attempt.buildGreen,
    summary: attempt.summary,
    artifact: judge ? { verdict: judge.verdict, rationale: judge.rationale } : null,
    friction: byCategory,
    softHolds,
  },
  task,
}
