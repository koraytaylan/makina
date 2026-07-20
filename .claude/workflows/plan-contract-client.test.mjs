import assert from 'node:assert/strict'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import fs from 'node:fs/promises'
import { PlanContractClient } from './plan-contract-client.js'

const root = await fs.mkdtemp(path.join(os.tmpdir(), 'makina-contract-client-'))
const endpoint = path.join(root, 'contract.sock')
const seen = []
const server = net.createServer(socket => {
  let input = ''
  socket.setEncoding('utf8')
  socket.on('data', chunk => {
    input += chunk
    if (!input.includes('\n')) return
    const envelope = JSON.parse(input.slice(0, input.indexOf('\n')))
    seen.push(envelope)
    const request = envelope.request
    const response = request.type === 'start_session'
      ? { type: 'ready', session_token: 'session', plan_oid: 'oid-r' }
      : request.type === 'claim_task'
        ? { type: 'task_claimed', task: request.task, plan_oid: 'oid-claim' }
        : { type: 'task_reconciled', evidence: { state: 'registration_only' } }
    socket.end(`${JSON.stringify(response)}\n`)
  })
})
await new Promise((resolve, reject) => server.listen(endpoint, resolve).once('error', reject))

try {
  const client = new PlanContractClient({ endpoint, authToken: 'secret' })
  await client.start({ repoRoot: '/repo', planDir: 'docs/plans/0050-x', planRef: 'refs/heads/plan/0050-x', runUid: 'run', expectedPlanOid: 'oid-r' })
  await Promise.all([
    client.claimTask({ task: 'task-a', lastUpdated: '2026-07-20' }),
    client.reconcileTask({ task: 'task-a' }),
  ])
  assert.deepEqual(seen.slice(1).map(item => item.request.mutation.request_id), [1, 2])
  assert.equal(seen[1].request.mutation.expected_plan_oid, 'oid-r')
  assert.equal(seen[2].request.mutation.expected_plan_oid, 'oid-claim')
  assert.ok(seen.every(item => item.auth_token === 'secret'))
  assert.ok(!JSON.stringify(seen).includes('writes'))
} finally {
  await new Promise(resolve => server.close(resolve))
  await fs.rm(root, { recursive: true, force: true })
}
