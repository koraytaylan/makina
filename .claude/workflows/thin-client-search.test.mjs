import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const files = ['create-plan.js', 'implement-plan.js']
const forbidden = [
  [/child_process|spawnSync|execSync/, 'process-level coordinator'],
  [/\bgit\s+(?:add|commit|merge|update-ref|worktree|reset|clean|branch)\b/i, 'direct repository mutation'],
  [/parse(?:Task|Frontmatter|Yaml)|yamlScalar|repoPatternMatches|parseNameStatus/i, 'duplicate parser or footprint matcher'],
  [/flock|hold-repository-lease\.py/, 'live lock implementation'],
  [/writeFile|appendFile|renameSync/, 'direct renderer/status writer'],
]
for (const file of files) {
  const source = readFileSync(new URL(`./${file}`, import.meta.url), 'utf8')
  for (const [pattern, label] of forbidden) assert.doesNotMatch(source, pattern, `${file} contains ${label}`)
}
console.log('workflow thin-client search gate passed')
