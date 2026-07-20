import { connectPlanContract } from './plan-contract-client.js'

export const meta = {
  name: 'implement-plan',
  description: 'Schedule dependency-ready workers while one Rust plan-contract session owns plan validation, repository exclusion, coordinator state, and integration.',
  phases: [
    { title: 'Open', detail: 'start or reconnect one authenticated contract session and reconcile durable evidence' },
    { title: 'Execute', detail: 'dispatch only contract-returned ready tasks with worker lifetime handles' },
    { title: 'Finalize', detail: 'ask the contract to perform the selected P/F/C transition and close stably' },
  ],
}

// <thin-client>
function normalizeImplementArgs(value) {
  if (typeof value === 'string') return { plan: value }
  return value && typeof value === 'object' && !Array.isArray(value) ? value : {}
}

function workerPrompt(task, worktree, findings) {
  return `Implement task ${task.id} in the dedicated worktree ${worktree}.\n`
    + `Task document: ${task.sourcePath}\nPlan scope: ${task.scopePath}\nPlan architecture: ${task.architecturePath}\n`
    + `Allowed footprint and gates are contract-issued in the task document. Coordinator documents are reserved. `
    + `Do not commit or perform repository topology operations.\n`
    + (findings ? `Resolve these reviewer findings:\n${JSON.stringify(findings)}` : '')
}

function reviewPrompt(task, worktree, report) {
  return `Independently review task ${task.id} in ${worktree}.\nTask document: ${task.sourcePath}\n`
    + `Re-run the authored gates and judge the Done-when criterion. Do not edit or perform repository topology operations.\n`
    + `Developer report: ${JSON.stringify(report)}`
}

async function withWorker(session, descriptor, invoke) {
  const begun = await session.beginWorker({ workerId: descriptor.workerId, mutation: descriptor.mutation })
  let result, failure
  try { result = await invoke() } catch (error) { failure = error }
  await session.endWorker({
    workerId: begun.workerId,
    mutation: begun.mutation,
    terminationEvidence: failure ? { outcome: 'failed', message: String(failure.message || failure) } : { outcome: 'completed' },
  })
  if (failure) throw failure
  return result
}

async function runTask(session, task, cfg) {
  const claim = await session.claimTask({ taskId: task.id })
  let findings = null
  const rounds = Math.max(1, Number(cfg.maxReviewIters) || 3)
  for (let round = 1; round <= rounds; round++) {
    const implementation = await withWorker(session, { workerId: `developer:${task.id}:${round}` }, () => agent(
      workerPrompt(task, claim.worktree, findings),
      { label: `developer:${task.id}:${round}`, phase: 'Execute', model: cfg.devModel || undefined, agentType: 'developer' },
    ))
    const candidate = await session.checkCandidate({ taskId: task.id, checkpoint: 'review' })
    const verdict = await withWorker(session, { workerId: `reviewer:${task.id}:${round}` }, () => agent(
      reviewPrompt(task, claim.worktree, implementation),
      { label: `reviewer:${task.id}:${round}`, phase: 'Execute', model: cfg.reviewModel || undefined, agentType: 'reviewer', schema: candidate.verdictSchema },
    ))
    if (verdict && verdict.approved && verdict.gatesPass) {
      return session.landTask({ taskId: task.id, candidateToken: candidate.token })
    }
    findings = verdict && verdict.findings || [{ severity: 'blocker', note: 'review did not approve' }]
  }
  return session.blockTask({ taskId: task.id, reason: 'review iteration limit reached', findings })
}

async function mapLimit(items, limit, fn) {
  const pending = items.slice(), results = []
  const workers = Array.from({ length: Math.min(limit, pending.length) }, async () => {
    while (pending.length) results.push(await fn(pending.shift()))
  })
  await Promise.all(workers)
  return results
}
// </thin-client>

const cfg = normalizeImplementArgs(args)
if (!cfg.plan) throw new Error('implement-plan requires a plan selector')
for (const field of ['contractEndpoint', 'contractAuthToken', 'repoRoot', 'planDir', 'planRef', 'runUid', 'expectedPlanOid', 'baseRef', 'retentionManifest']) {
  if (!cfg[field]) throw new Error(`implement-plan requires ${field}`)
}

phase('Open')
const session = connectPlanContract({ endpoint: cfg.contractEndpoint, authToken: cfg.contractAuthToken })
try {
  await session.hello()
  await session.start({ repoRoot: cfg.repoRoot, planDir: cfg.planDir, planRef: cfg.planRef,
    runUid: cfg.runUid, expectedPlanOid: cfg.expectedPlanOid })
  const inspected = await session.inspectPlan()
  const snapshot = inspected.plan
  if (cfg.dryRun) {
    await session.cancelSession()
    return snapshot
  }

  phase('Execute')
  const results = []
  const parallelism = cfg.sequential ? 1 : Math.max(1, Number(cfg.maxParallel) || 4)
  while (true) {
    const wave = await session.readyTasks()
    if (wave.complete || wave.blocked || wave.tasks.length === 0) break
    results.push(...await mapLimit(wave.tasks, parallelism, task => runTask(session, task, cfg)))
  }

  phase('Finalize')
  const mode = cfg.finalization === 'automatic' || !cfg.finalization ? 'squash' : cfg.finalization
  const prepared = await session.prepareFinalization({ mode, lastUpdated: cfg.lastUpdated || new Date().toISOString().slice(0, 10) })
  if (mode === 'stage' || mode === 'manual') return { plan: snapshot.plan_dir, results, finalization: prepared, cleanupPermit: null }
  await session.integrateFinalization()
  const finalization = await session.completeFinalization({ lastUpdated: cfg.lastUpdated || new Date().toISOString().slice(0, 10) })
  const completion = { base_ref: cfg.baseRef, completion_oid: finalization.completion_oid,
    final_oid: finalization.final_oid, plan: cfg.planDir.split('/').pop(), run_uid: cfg.runUid }
  const closed = await session.close({ completion, retentionManifest: cfg.retentionManifest })
  return { plan: snapshot.plan_dir, results, finalization, cleanupPermit: closed.cleanup_permit || null }
} catch (error) {
  try { await session.cancelSession() } catch {}
  throw error
}
