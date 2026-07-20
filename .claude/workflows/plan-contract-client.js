import net from 'node:net'

const PROTOCOL = 1

function diagnostic(response) {
  if (response && response.type === 'error') {
    const error = new Error(response.diagnostic.message)
    error.code = response.diagnostic.code
    throw error
  }
  return response
}

export class PlanContractClient {
  constructor({ endpoint, authToken }) {
    if (!endpoint || !authToken) throw new Error('endpoint and authToken are required')
    this.endpoint = endpoint
    this.authToken = authToken
    this.sessionToken = null
    this.sourceOid = null
    this.planOid = null
    this.requestId = 0
    this.workerHandles = new Map()
    this.mutationTail = Promise.resolve()
  }

  request(request) {
    return new Promise((resolve, reject) => {
      const socket = net.createConnection(this.endpoint)
      let input = '', settled = false
      const finish = (error, value) => {
        if (settled) return
        settled = true
        socket.destroy()
        error ? reject(error) : resolve(value)
      }
      socket.setEncoding('utf8')
      socket.on('connect', () => socket.write(`${JSON.stringify({ auth_token: this.authToken, request })}\n`))
      socket.on('data', chunk => {
        input += chunk
        const newline = input.indexOf('\n')
        if (newline < 0) return
        try { finish(null, diagnostic(JSON.parse(input.slice(0, newline)))) }
        catch (error) { finish(error) }
      })
      socket.on('error', error => finish(error))
      socket.on('end', () => finish(new Error('plan-contract closed without a response')))
    })
  }

  async hello() { return this.request({ type: 'hello', protocol: PROTOCOL }) }

  async start({ repoRoot, planDir, planRef, runUid, expectedPlanOid }) {
    const response = await this.request({
      type: 'start_session', protocol: PROTOCOL,
      repo_root: repoRoot, plan_dir: planDir, plan_ref: planRef,
      run_uid: runUid, expected_plan_oid: expectedPlanOid,
    })
    if (response.type !== 'ready') throw new Error(`expected ready, received ${response.type}`)
    this.sessionToken = response.session_token
    this.sourceOid = response.plan_oid
    this.planOid = response.plan_oid
    return response
  }

  async startAuthoring({ repoRoot, baseBranch, runUid, expectedBaseOid }) {
    const response = await this.request({ type: 'start_authoring_session', protocol: PROTOCOL,
      repo_root: repoRoot, base_branch: baseBranch, run_uid: runUid,
      expected_base_oid: expectedBaseOid })
    if (response.type !== 'authoring_ready') throw new Error(`expected authoring_ready, received ${response.type}`)
    this.sessionToken = response.session_token
    this.sourceOid = response.base_oid
    this.planOid = response.base_oid
    return response
  }

  mutation() {
    if (!this.sessionToken) throw new Error('session is not started')
    return {
      session_token: this.sessionToken, request_id: ++this.requestId,
      expected_source_oid: this.sourceOid, expected_plan_oid: this.planOid,
    }
  }

  mutate(type, fields = {}) {
    const operation = this.mutationTail.then(async () => {
      const response = await this.request({ type, mutation: this.mutation(), ...fields })
      if (response.plan_oid) this.planOid = response.plan_oid
      return response
    })
    this.mutationTail = operation.catch(() => {})
    return operation
  }

  inspectPlan() { return this.mutate('inspect_plan') }
  inspectAuthoring({ count = 1 } = {}) { return this.mutate('inspect_authoring', { count }) }
  publishBlueprint({ reservation, blueprint, commit = false }) { return this.mutate('publish_blueprint', { reservation, blueprint, commit }) }
  closeAuthoring() { return this.mutate('close_authoring') }
  async beginAuthorWorker({ workerId }) {
    const response = await this.mutate('begin_author_worker', { worker_id: workerId })
    this.workerHandles.set(workerId, response.termination_handle)
    return { ...response, workerId: response.worker_id }
  }
  async endAuthorWorker({ workerId, terminationEvidence }) {
    const termination_handle = this.workerHandles.get(workerId)
    if (!termination_handle) throw new Error(`no live author worker handle for ${workerId}`)
    const response = await this.mutate('end_author_worker', {
      worker_id: workerId,
      termination_evidence: { termination_handle, ...terminationEvidence },
    })
    this.workerHandles.delete(workerId)
    return response
  }
  cancelSession() { return this.mutate('cancel_session') }
  async readyTasks() {
    const response = await this.mutate('ready_tasks')
    return { ...response, tasks: response.tasks.map(task => ({
      ...task, sourcePath: task.source_path, scopePath: task.scope_path,
      architecturePath: task.architecture_path,
    })) }
  }
  async checkCandidate({ task }) {
    const response = await this.mutate('check_candidate', { task })
    return { ...response, token: response.candidate_token }
  }
  landTask({ task, candidateToken, lastUpdated = new Date().toISOString().slice(0, 10) }) {
    return this.mutate('land_task', { task, candidate_token: candidateToken, last_updated: lastUpdated })
  }
  reconcileTask({ task }) { return this.mutate('reconcile_task', { task }) }
  claimTask({ task, lastUpdated = new Date().toISOString().slice(0, 10) }) { return this.mutate('claim_task', { task, last_updated: lastUpdated }) }
  landPhaseA({ task }) { return this.mutate('land_phase_a', { task }) }
  commitPhaseB({ task, lastUpdated }) { return this.mutate('commit_phase_b', { task, last_updated: lastUpdated }) }
  transitionTask({ task, transition }) { return this.mutate('transition_task', { task, transition }) }
  blockTask({ task, reason }) { return this.transitionTask({ task, transition: { action: 'block', reason } }) }
  setDisposition({ task, disposition }) { return this.mutate('set_disposition', { task, disposition }) }
  prepareFinalization({ mode, lastUpdated }) { return this.mutate('prepare_finalization', { mode, last_updated: lastUpdated }) }
  integrateFinalization({ manualOid = null } = {}) { return this.mutate('integrate_finalization', { manual_oid: manualOid }) }
  completeFinalization({ lastUpdated }) { return this.mutate('complete_finalization', { last_updated: lastUpdated }) }

  async beginWorker({ workerId, process = null }) {
    const fields = { worker_id: workerId }
    if (process) fields.process = process
    const response = await this.mutate('begin_worker', fields)
    this.workerHandles.set(workerId, response.termination_handle)
    return { ...response, workerId: response.worker_id }
  }
  async endWorker({ workerId, terminationEvidence }) {
    const termination_handle = this.workerHandles.get(workerId)
    if (!termination_handle) throw new Error(`no live worker handle for ${workerId}`)
    const response = await this.mutate('end_worker', {
      worker_id: workerId,
      termination_evidence: { termination_handle, ...terminationEvidence },
    })
    this.workerHandles.delete(workerId)
    return response
  }
  beginGitChild({ childId, process }) { return this.mutate('begin_git_child', { child_id: childId, process }) }
  endGitChild({ childId, terminationEvidence }) {
    return this.mutate('end_git_child', { child_id: childId, wait_evidence: terminationEvidence })
  }
  close({ completion, retentionManifest }) {
    return this.mutate('close', { completion, retention_manifest: retentionManifest })
  }
}

export function connectPlanContract(options) { return new PlanContractClient(options) }
