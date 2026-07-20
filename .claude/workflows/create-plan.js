import { connectPlanContract } from './plan-contract-client.js'

export const meta = {
  name: 'create-plan',
  description: 'Research blueprint content and delegate canonical plan creation and optional registration to the Rust plan-contract.',
  phases: [
    { title: 'Inspect', detail: 'obtain authoring context and collision-free reservations from the typed contract' },
    { title: 'Blueprint', detail: 'ask an author agent for structured content only' },
    { title: 'Publish', detail: 'delegate canonical rendering, validation, optional commit, and registration' },
  ],
}

// Workflow hosts provide this typed adapter. It owns the authenticated server process,
// reconnectable endpoint, protocol envelope, request sequence, and shutdown/recovery.
// JavaScript deliberately contains no plan parser, renderer, number allocator, lock,
// repository mutation, or status implementation.
// <thin-client>
function normalizeCreateArgs(value) {
  if (typeof value === 'string') return { brief: value }
  return value && typeof value === 'object' && !Array.isArray(value) ? value : {}
}

function blueprintPrompt(context, cfg) {
  return `Research the repository and return only a structured plan blueprint for the Rust plan-contract.\n`
    + `Brief: ${cfg.brief || 'derive the highest-priority recorded follow-on'}\n`
    + `Reservation/context: ${JSON.stringify(context)}\n`
    + `Provide exactly title, slug, scope, architecture, initial_status, workstreams, and tasks. `
    + `initial_status needs goal, root_cause, approach, outcome, and last_updated (YYYY-MM-DD). `
    + `Each task needs a four-digit sequence, plan-local kebab id, title, workstream, kind, dependency ids, gated boolean, portable repository-relative touches, body steps, and a falsifiable Done-when criterion. `
    + `Do not write files, choose numbers, update status, or run repository mutations.`
}

async function withAuthorWorker(session, workerId, invoke) {
  const begun = await session.beginAuthorWorker({ workerId })
  try {
    const result = await invoke()
    await session.endAuthorWorker({
      workerId: begun.workerId,
      terminationEvidence: { outcome: 'completed' },
    })
    return result
  } catch (error) {
    await session.endAuthorWorker({
      workerId: begun.workerId,
      terminationEvidence: { outcome: 'failed', message: String(error?.message || error) },
    })
    throw error
  }
}

// </thin-client>

const cfg = normalizeCreateArgs(args)
const count = Math.max(1, Math.min(5, Number(cfg.count) || 1))
for (const field of ['contractEndpoint', 'contractAuthToken', 'repoRoot', 'baseBranch', 'expectedBaseOid']) {
  if (!cfg[field]) throw new Error(`create-plan requires ${field}`)
}

phase('Inspect')
const session = connectPlanContract({ endpoint: cfg.contractEndpoint, authToken: cfg.contractAuthToken })
try {
  await session.hello()
  await session.startAuthoring({ repoRoot: cfg.repoRoot, baseBranch: cfg.baseBranch,
    runUid: String(cfg.runUid || `author-${Date.now()}`), expectedBaseOid: cfg.expectedBaseOid })
  const context = await session.inspectAuthoring({ count })
  if (cfg.dryRun) { await session.closeAuthoring(); return { dryRun: true, ...context } }

  phase('Blueprint')
  const blueprints = []
  for (const [index, reservation] of context.reservations.entries()) {
    const blueprint = await withAuthorWorker(session, `blueprint:${index + 1}`, () => agent(blueprintPrompt(reservation, cfg), {
      label: `blueprint:${index + 1}`,
      phase: 'Blueprint',
      model: cfg.authorModel || undefined,
      effort: 'high',
    }))
    if (!blueprint) throw new Error(`blueprint agent returned no result for candidate ${index + 1}`)
    blueprints.push({ reservation, blueprint })
  }

  phase('Publish')
  const results = []
  for (const candidate of blueprints) results.push(await session.publishBlueprint({ ...candidate, commit: cfg.commit === true }))
  await session.closeAuthoring()
  return { results }
} catch (error) {
  try { await session.closeAuthoring() } catch {}
  throw error
}
