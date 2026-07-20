import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const source = readFileSync(new URL('./create-plan.js', import.meta.url), 'utf8')
const start = source.indexOf('// <thin-client>')
const end = source.indexOf('// </thin-client>')
assert.ok(start >= 0 && end > start, 'thin client helper block is missing')
const load = new Function(`${source.slice(start, end)}; return { normalizeCreateArgs, blueprintPrompt, withAuthorWorker }`)
const { normalizeCreateArgs, blueprintPrompt, withAuthorWorker } = load()

assert.deepEqual(normalizeCreateArgs('cache eviction'), { brief: 'cache eviction' })
assert.deepEqual(normalizeCreateArgs(null), {})
const prompt = blueprintPrompt({ number: '0051' }, { brief: 'cache eviction' })
assert.match(prompt, /structured plan blueprint/)
assert.match(prompt, /Do not write files/)

const calls = []
const session = {
  beginAuthorWorker: async request => { calls.push(['begin', request]); return { workerId: request.workerId } },
  endAuthorWorker: async request => { calls.push(['end', request]) },
}
assert.equal(await withAuthorWorker(session, 'blueprint:1', async () => {
  calls.push(['agent'])
  return 'blueprint'
}), 'blueprint')
assert.deepEqual(calls.map(call => call[0]), ['begin', 'agent', 'end'])
assert.equal(calls[2][1].terminationEvidence.outcome, 'completed')

await assert.rejects(withAuthorWorker(session, 'blueprint:2', async () => { throw new Error('boom') }), /boom/)
assert.equal(calls.at(-1)[1].terminationEvidence.outcome, 'failed')

console.log('create-plan thin contract client checks passed')
