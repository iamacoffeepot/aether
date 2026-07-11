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
//   authorEffort, string  — reasoning effort for the Author (default 'medium').
//   attemptEffort,string  — reasoning effort for the Attempt (default 'high' — it does real engineering).
//   judgeEffort,  string  — reasoning effort for the judge (default 'high').
//   driverModel,  string  — the model of the harness session that INVOKED this workflow. The workflow
//                           cannot read it, so an unattended caller (the CI action) passes it in purely
//                           so the rollup can record it. Recorded, never acted on.
//   judgeFramePath, string — absolute path the JUDGE saves the frame it actually grades to. The evidence
//                           image was previously a later re-capture by the caller, which can disagree with
//                           what the judge saw; this makes the verdict auditable against its own pixels.
// }
//
// returns { proposedTask?, needsApproval?, rollup, task }. For a heavy medium (author / build-layer)
// authored fresh, the run STOPS after Author and returns { proposedTask, needsApproval:true, rollup:null }
// — a workflow cannot block on human input, so the caller reviews the task and re-invokes with args.task.
// A drive task runs straight through. rollup = { totals, succeeded, buildGreen, artifact, friction
// (grouped papercut|missing-primitive|doc-gap|blocker), softHolds, task, provenance }. softHolds = a wrong
// artifact verdict or any high-severity blocker — the subset a reviewer clears. friction feeds the
// flywheel: papercut -> /sketch, missing-primitive -> a build-machinery issue, doc-gap -> a guide edit.
//
// rollup.task and rollup.provenance exist so a trial is DIAGNOSABLE from its evidence alone. A verdict
// read without the task that produced it, and without the prompt/model/effort of the agent that rendered
// it, is unfalsifiable: a reader cannot tell a real defect from an agent that was asked the wrong
// question. They ride inside the rollup (not as a sibling of it) because the rollup is the single file
// the evidence branch, the viewer, and the issue comment all read.

const A = (typeof args === 'string') ? JSON.parse(args || '{}') : (args || {})
const AUTHOR_MODEL = A.authorModel || 'sonnet'
const ATTEMPT_MODEL = A.attemptModel || 'opus'
const JUDGE_MODEL = A.judgeModel || 'opus'
const AUTHOR_EFFORT = A.authorEffort || 'medium'
const ATTEMPT_EFFORT = A.attemptEffort || 'high'
const JUDGE_EFFORT = A.judgeEffort || 'high'

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

// One mail in the replay bundle — the shape capture_frame's own `mails` parameter takes, so the
// Attempt's reported bundle passes through to the judge's capture verbatim.
const REPLAY_MAIL = {
  type: 'object',
  additionalProperties: false,
  required: ['recipient_name', 'kind_name'],
  properties: {
    recipient_name: { type: 'string', description: 'the mailbox to send to on the live engine (e.g. "aether.render", "aether.text")' },
    kind_name: { type: 'string', description: 'the kind name of the payload (e.g. "aether.draw_triangle", "aether.text.draw_batch")' },
    params: { description: 'the kind\'s params, exactly as you sent them; omit for a fieldless kind' },
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
    replayMails: { type: 'array', items: REPLAY_MAIL, description: 'for a render task: the mail bundle that re-establishes the expected visual state on the live engine, because the render surface is immediate-mode and one-shot draws do not survive the frame they were sent in. The judge dispatches it in its own capture. Empty when the visual state redraws itself every tick (a loaded component) or when nothing renders.' },
    buildGreen: { type: ['boolean', 'null'], description: 'author / build-layer: did the scratch crate build (and any tests pass); null for drive' },
    findings: { type: 'array', items: FINDING_ITEM },
  },
}

const JUDGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['verdict', 'rationale'],
  properties: {
    verdict: {
      type: 'string',
      enum: ['correct', 'wrong', 'insufficient-evidence', 'n-a'],
      description: 'correct = the frame satisfies the rubric; wrong = it contradicts the rubric (use-visible defect); insufficient-evidence = the rubric cannot be settled from the evidence given (a comparison with no baseline, a claim about bytes or dimensions, a state not visible in this frame); n-a = nothing renderable to judge',
    },
    rationale: { type: 'string', description: 'what the frame showed vs what was expected — the concrete visual discrepancy for a wrong verdict, or exactly which evidence was missing for insufficient-evidence' },
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

${task.expectedArtifact ? `RENDER ARTIFACT: this task renders. After you produce the visual state, spawn_substrate / drive it so the expected frame is showing, and LEAVE THE ENGINE ALIVE — report its engine_id so a judge can capture it. Do NOT terminate that engine.

THE IMMEDIATE-MODE RULE (load-bearing — the judge grades a frame it captures itself, later):
- The render surface is immediate mode. draw_triangle, draw_textured_quads, and the aether.text.draw* family accumulate for ONE frame and clear. Mailing them once does not leave a lasting picture: a bare capture taken after the fact comes back empty, and the judge would rule your correct render 'wrong'.
- So report replayMails: the exact mail bundle a fresh capture must dispatch to put the expected frame back on screen — each entry { recipient_name, kind_name, params } exactly as you sent it. The judge passes it as capture_frame's own \`mails\`, which fire before readback.
- Report replayMails = [] ONLY when the picture genuinely redraws itself every tick (a loaded component that re-sends its draws on Tick). If you produced the visual with one-shot mail, an empty bundle means a blank frame and a false verdict.` : `No render artifact: terminate_substrate any engine you spawned before returning; report engineId = null and replayMails = [].`}

Return succeeded, summary, engineId, replayMails, buildGreen, and findings (every friction point — category, severity, where, what, suggested). An empty findings array means the surface was friction-free; be honest, not generous.`
}

// The judge is the only agent in the pipeline with authority to call a landed feature visibly broken, and
// it was the only one given nothing to go on. It received the rubric and an engine id — not the task, not
// what the consumer says it did, and no baseline — so on a rubric it could not settle it filled the gap
// with a prior about what the domain "ought" to look like and reported the gap as a defect. Replaying the
// shipped prompt over ten archived frames scored 24/30 against hand labels, with three false `wrong`
// verdicts; the text below scored 30/30 with none. Both were perfectly self-consistent across three
// samples, so the failure was never noise — it was a question the judge could not answer being posed as
// one it had to. Re-score against the frame corpus before editing this prompt.
function judgePrompt(task, engineId, replayMails, attemptSummary, framePath) {
  const replay = (replayMails && replayMails.length)
    ? `The engine's render surface is immediate mode — the consumer's draws cleared the frame after they were sent, so a bare capture would come back blank. Pass this bundle as capture_frame's \`mails\` (it fires before readback, putting the picture back on screen for the frame you read):

${JSON.stringify(replayMails, null, 2)}`
    : `The consumer reported no replay bundle — the visual state redraws itself every tick — so a plain capture (no \`mails\`) shows it.`

  // The frame the judge graded was never archived: the evidence image is a LATER re-capture by the CI
  // driver, and the two have been observed to disagree (a `wrong` verdict posted directly above a frame
  // that plainly satisfies its rubric). Saving the judged frame makes the verdict auditable against the
  // pixels that produced it, rather than against a different picture of the same engine.
  const save = framePath
    ? `\n- \`save_path\`: "${framePath}" — persist the exact frame you grade, so the verdict can be audited against the pixels that produced it.`
    : ''

  return `You are a vision judge for a dogfood trial. A consumer agent drove the aether engine to produce a visual result and left it alive for you. Your job is to rule whether the frame it produced is use-visibly correct.

THE TASK THE CONSUMER WAS HANDED:
${task.prompt}

WHAT THE CONSUMER REPORTS IT DID (its own account — treat as context, not as proof):
${attemptSummary}

EXPECTED ARTIFACT (the rubric — grade against THIS TEXT and nothing else):
${task.expectedArtifact}

${replay}

Call capture_frame on engine_id "${engineId}" (ToolSearch for mcp__aether-hub__capture_frame). The PNG returns inline — LOOK at it.${save}

HOW TO RULE:
- Grade ONLY against the rubric as written. Do NOT import an assumption about what this kind of scene
  "ought" to look like. If the rubric says an authored region should differ from a baseline, a bare shape
  on an empty background may be exactly that — the engine renders only what the consumer put there, and a
  scene that starts empty stays empty. A noun in the task (terrain, panel, list) is not a promise of
  scenery you have priors about.
- THE SINGLE-FRAME RULE (load-bearing): you have ONE image. If the rubric asks you to compare states — a
  preview versus a baseline, a before versus an after, several captures at different scales, pixel
  dimensions across images — you CANNOT settle that from one frame. Do not guess, and do not resolve the
  gap by assuming the missing frame was wrong. Return 'insufficient-evidence' and name exactly which
  evidence you were not given.
- verdict 'correct' — the frame satisfies what the rubric actually asks, on the evidence you have.
- verdict 'wrong' — the frame contradicts the rubric: the thing that was supposed to render is absent or
  visibly malformed. Name the concrete visual discrepancy.
- verdict 'insufficient-evidence' — the rubric cannot be settled from what you were given. This is a real
  verdict, not a hedge: use it when it is true, and do not use it to avoid a call you CAN make.
- verdict 'n-a' — nothing renderable came back at all.

After you have judged, terminate_substrate engine_id "${engineId}" to free the fleet.

Return verdict and rationale.`
}

// Phase 1 — Author (skipped when a task is supplied). A freshly-authored heavy-medium task is
// human-gated: the run returns the proposal and stops, because a workflow cannot block on input.
// Every prompt actually sent is retained verbatim for the rollup's provenance block — the reader of a
// verdict needs the question the agent was asked, not a reconstruction of it.
const prompts = { author: null, attempt: null, judge: null }

let task = A.task || null
if (!task) {
  phase('Author')
  prompts.author = authorPrompt(A.issue, A.diff, A.surface, A.medium)
  task = await agent(prompts.author, {
    label: 'author', phase: 'Author', model: AUTHOR_MODEL, effort: AUTHOR_EFFORT, schema: TASK_SCHEMA,
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
prompts.attempt = attemptPrompt(task)
const attempt = await agent(prompts.attempt, {
  label: `attempt:${task.medium}`, phase: 'Attempt', model: ATTEMPT_MODEL, effort: ATTEMPT_EFFORT,
  agentType: 'general-purpose',
  isolation: HEAVY.has(task.medium) ? 'worktree' : undefined,
  schema: FRICTION_SCHEMA,
})
if (!attempt) throw new Error('dogfood: the Attempt agent died with no friction report')

// Phase 3 — Judge (render artifact only). The judge re-captures the still-alive engine itself, so the
// PNG lands in its own vision context — no file handoff, no agent grading its own screenshot. The
// render surface is immediate-mode, so a one-shot draw is gone by the time the judge looks: it
// dispatches the Attempt's replay bundle as capture_frame's `mails` to put the picture back in the
// frame it reads. It terminates the engine when done.
let judge = null
if (task.expectedArtifact && attempt.engineId) {
  phase('Judge')
  prompts.judge = judgePrompt(task, attempt.engineId, attempt.replayMails, attempt.summary, A.judgeFramePath)
  judge = await agent(prompts.judge, {
    label: 'judge', phase: 'Judge', model: JUDGE_MODEL, effort: JUDGE_EFFORT,
    agentType: 'general-purpose', schema: JUDGE_SCHEMA,
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

// What ran, and what it was asked. A phase that did not run (Author on a supplied task, Judge on a
// non-rendering task) records a null prompt rather than being omitted, so the reader can tell "this phase
// was skipped" from "this field was never recorded".
//
// The stored prompt is clipped: the Author's embeds the entire landed diff, and the rollup is committed
// to the evidence branch on every trial. The clip is generous enough that the Attempt's and the Judge's
// prompts — the two a reader actually adjudicates a verdict against — always land whole, and it marks
// itself when it fires rather than truncating silently.
const PROMPT_STORE_MAX = 20000
const storedPrompt = (p) => (p === null || p.length <= PROMPT_STORE_MAX)
  ? p
  : `${p.slice(0, PROMPT_STORE_MAX)}\n\n[…clipped ${p.length - PROMPT_STORE_MAX} chars of ${p.length} for the evidence record]`

const provenance = {
  driver: { model: A.driverModel || null },
  phases: {
    author: { model: AUTHOR_MODEL, effort: AUTHOR_EFFORT, agentType: null, prompt: storedPrompt(prompts.author) },
    attempt: { model: ATTEMPT_MODEL, effort: ATTEMPT_EFFORT, agentType: 'general-purpose', prompt: storedPrompt(prompts.attempt) },
    judge: { model: JUDGE_MODEL, effort: JUDGE_EFFORT, agentType: 'general-purpose', prompt: storedPrompt(prompts.judge) },
  },
}

log(`dogfood [${task.medium}]: succeeded=${attempt.succeeded}${attempt.buildGreen === null ? '' : ` buildGreen=${attempt.buildGreen}`}${judge ? ` artifact=${judge.verdict}` : ''} — ${totals.findings} findings (papercut ${totals.papercut}, missing-primitive ${totals.missingPrimitive}, doc-gap ${totals.docGap}, blocker ${totals.blocker}), ${softHolds.length} SOFT-HOLD`)
log(`dogfood provenance: driver=${provenance.driver.model || 'unrecorded'} attempt=${ATTEMPT_MODEL}/${ATTEMPT_EFFORT} judge=${JUDGE_MODEL}/${JUDGE_EFFORT}`)

return {
  rollup: {
    totals,
    succeeded: attempt.succeeded,
    buildGreen: attempt.buildGreen,
    summary: attempt.summary,
    artifact: judge ? { verdict: judge.verdict, rationale: judge.rationale } : null,
    friction: byCategory,
    softHolds,
    task,
    provenance,
  },
  task,
}
