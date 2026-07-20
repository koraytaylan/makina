import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawn, spawnSync } from 'node:child_process'

const dir = mkdtempSync(join(tmpdir(), 'makina-lease-'))
const script = new URL('../../crates/makina-core/tests/fixtures/repository-lease/inert-flock-holder.py', import.meta.url).pathname
const waitLine = child => new Promise((resolve, reject) => { let s=''; child.stdout.on('data', d => { s += d; if (s.includes('\n')) resolve(JSON.parse(s.split('\n')[0])) }); child.on('error', reject) })
try {
  const holder = spawn('python3',[script,'--git-common-dir',dir,'--run','one'],{stdio:['pipe','pipe','inherit']})
  assert.equal((await waitLine(holder)).event,'ready')
  const contender = spawnSync('python3',[script,'--git-common-dir',dir,'--run','two','--nonblocking'],{input:'',encoding:'utf8'})
  assert.equal(contender.status,75); assert.match(contender.stdout,/contended/)
  holder.stdin.write('{"op":"release"}\n'); await new Promise(r => holder.on('exit',r))
  const next = spawnSync('python3',[script,'--git-common-dir',dir,'--run','three','--nonblocking'],{input:'{"op":"release"}\n',encoding:'utf8'})
  assert.equal(next.status,0); assert.match(next.stdout,/ready/); assert.equal(readFileSync(join(dir,'makina.repository.lock')).length,0)
  console.log('repository lease checks passed')
} finally { rmSync(dir,{recursive:true,force:true}) }
