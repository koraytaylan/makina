import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const source = readFileSync(new URL('./implement-plan.js', import.meta.url), 'utf8')
const start = source.indexOf('// <thin-client>')
const end = source.indexOf('// </thin-client>')
assert.ok(start >= 0 && end > start, 'thin client helper block is missing')
const load = new Function(`${source.slice(start, end)}; return { normalizeImplementArgs, withWorker, mapLimit }`)
const { normalizeImplementArgs, withWorker, mapLimit } = load()

assert.deepEqual(normalizeImplementArgs('0048'), { plan: '0048' })
const calls = []
const session = {
  beginWorker: async request => { calls.push(['begin', request]); return { workerId: request.workerId, mutation: { requestId: 2 } } },
  endWorker: async request => { calls.push(['end', request]); },
}
assert.equal(await withWorker(session, { workerId: 'dev-a', mutation: { requestId: 1 } }, async () => 'ok'), 'ok')
assert.deepEqual(calls.map(call => call[0]), ['begin', 'end'])
assert.equal(calls[1][1].terminationEvidence.outcome, 'completed')

await assert.rejects(withWorker(session, { workerId: 'dev-b', mutation: {} }, async () => { throw new Error('boom') }), /boom/)
assert.equal(calls.at(-1)[1].terminationEvidence.outcome, 'failed')
const values = await mapLimit([1, 2, 3], 2, async value => value * 2)
assert.deepEqual(values.sort(), [2, 4, 6])
console.log('implement-plan thin contract client checks passed')
