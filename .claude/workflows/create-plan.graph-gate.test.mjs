// Regression test for create-plan's authoring-side dependency-graph gate.
//
// It loads the PURE graph-validation functions straight out of create-plan.js (the block fenced by the
// `// <graph-gate>` / `// </graph-gate>` sentinels) so there is a single source of truth — the test
// exercises the exact code the workflow ships. No workflow runtime is needed.
//
// Run:  node .claude/workflows/create-plan.graph-gate.test.mjs
//
// Covers the two defect classes that slipped past plan 0031's authoring VERIFY step:
//   (a) a dependency CYCLE, (b) a "Depends on" edge that points to a LATER-declared task, and
//   (c) a best-effort producer-after-consumer symbol ordering — plus a clean plan must stay clean.

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import assert from 'node:assert/strict'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = join(here, '..', '..')

// ── Load the gate functions from create-plan.js (between the sentinels) ──────────
const src = readFileSync(join(here, 'create-plan.js'), 'utf8')
const start = src.indexOf('// <graph-gate>')
const end = src.indexOf('// </graph-gate>')
assert.ok(start !== -1 && end !== -1 && end > start, 'could not find the // <graph-gate> … // </graph-gate> block in create-plan.js')
const block = src.slice(start, end)
const load = new Function(`${block}\n;return { parseTasksGraph, parseDepList, codeOf, findCycle, analyzeDepOrder, cycleHasEdge, symbolInversions, analyzeTaskGraph };`)
const { parseTasksGraph, parseDepList, analyzeTaskGraph } = load()

let passed = 0
const check = (name, fn) => { fn(); passed++; console.log(`  ✓ ${name}`) }

// ── Unit: parseDepList ──────────────────────────────────────────────────────────
check('parseDepList treats em/en/hyphen dashes, none, n/a, empty as no-deps', () => {
  for (const s of ['—', '–', '-', '', 'none', 'None', 'N/A', 'tbd']) assert.deepEqual(parseDepList(s), [])
})
check('parseDepList splits comma lists, strips backticks + trailing period', () => {
  assert.deepEqual(parseDepList('`add-foo`, add-bar , baz.'), ['add-foo', 'add-bar', 'baz'])
})
check('parseDepList tolerates the house-style prose annotations on a Depends-on line', () => {
  // a comma INSIDE a parenthetical must not fragment into bogus ids
  assert.deepEqual(parseDepList('retry-redispatch (and plan 0016 merged, on the base branch)'), ['retry-redispatch'])
  // a "none" with a trailing caveat collapses to no deps
  assert.deepEqual(parseDepList('— (but **requires plan 0016 merged**: uses tree_move)'), [])
  // trailing prose after a real id is dropped to just the leading id
  assert.deepEqual(parseDepList('role-turn-metrics-event (and plan 0021 header layout)'), ['role-turn-metrics-event'])
})

// ── Unit: parseTasksGraph ignores workstream headers, reads ids + deps in order ──
check('parseTasksGraph keeps file order and parses Depends-on ids', () => {
  const md = [
    '# Plan', '', '## 0001 — WS', '',
    '### a-task — A', 'ctx', '- **Depends on:** —', '- **Done when:** green.', '',
    '### b-task — B (GATED)', 'ctx', '- **Depends on:** a-task', '- **Done when:** green.',
  ].join('\n')
  const tasks = parseTasksGraph(md)
  assert.deepEqual(tasks.map(t => t.id), ['a-task', 'b-task'])
  assert.deepEqual(tasks[0].deps, [])
  assert.deepEqual(tasks[1].deps, ['a-task'])
  assert.equal(tasks[1].gated, true)
})

// ── (a) a deliberately CYCLIC TASKS.md is rejected ──────────────────────────────
check('a deliberately-cyclic TASKS.md is REJECTED with a cycle finding', () => {
  const md = [
    '# Plan 9999 — Cyclic', '', '## 0001 — WS', '',
    '### task-a — Task A', 'body', '- **Depends on:** task-b', '- **Done when:** green.', '', '---', '',
    '### task-b — Task B', 'body', '- **Depends on:** task-a', '- **Done when:** green.',
  ].join('\n')
  const res = analyzeTaskGraph(md)
  assert.ok(res.cycle, 'expected a cycle to be detected')
  assert.ok(res.findings.length >= 1, 'a cyclic plan must produce at least one blocker finding')
  assert.ok(res.findings.some(f => /CYCLE/.test(f.note)), 'expected a CYCLE finding')
})

// ── (b) an edge pointing to a LATER-declared task is rejected ────────────────────
check('a later-pointing "Depends on" edge is flagged as a file-order inversion', () => {
  const md = [
    '## 0001 — WS', '',
    '### early — Early', 'body', '- **Depends on:** late', '- **Done when:** green.', '', '---', '',
    '### late — Late', 'body', '- **Depends on:** —', '- **Done when:** green.',
  ].join('\n')
  const res = analyzeTaskGraph(md)
  assert.ok(!res.cycle, 'this fixture is acyclic (only a backward edge)')
  assert.ok(res.findings.some(f => /file-order INVERSION/.test(f.note) && /early/.test(f.note) && /late/.test(f.note)), 'expected a file-order inversion for early→late')
})

// ── (c) a consumer declared before its unique producer is flagged ───────────────
check('a symbol used before its definition is flagged (best-effort)', () => {
  const md = [
    '## 0001 — WS', '',
    '### use-bar — Use Bar', 'consumes it', '**Steps:**', '1. use', '```rust', 'let b = BarThing::default();', '```',
    '- **Depends on:** —', '- **Done when:** green.', '', '---', '',
    '### add-bar — Add Bar', 'defines it', '**Steps:**', '1. add', '```rust', 'pub struct BarThing { x: u32 }', '```',
    '- **Depends on:** —', '- **Done when:** green.',
  ].join('\n')
  const res = analyzeTaskGraph(md)
  assert.ok(res.findings.some(f => /symbol-order INVERSION/.test(f.note) && /BarThing/.test(f.note)), 'expected a symbol-order inversion for BarThing')
})

// ── A clean, file-order-topological plan must stay CLEAN (no false positives) ────
check('a correctly ordered plan produces ZERO findings', () => {
  const md = [
    '# Plan — Clean', '', '## 0001 — WS', '',
    '### add-foo-config — Add FooConfig', 'defines', '**Steps:**', '1. add', '```rust', 'pub struct FooConfig { pub x: u32 }', '```',
    '- **Depends on:** —', '- **Done when:** green.', '', '---', '',
    '### use-foo-config — Use FooConfig', 'uses', '**Steps:**', '1. use', '```rust', 'let f = FooConfig { x: 1 };', '```',
    '- **Depends on:** add-foo-config', '- **Done when:** green.',
  ].join('\n')
  const res = analyzeTaskGraph(md)
  assert.equal(res.findings.length, 0, `clean plan should have no findings, got: ${res.findings.map(f => f.note).join(' | ')}`)
})

// ── The REAL defective plan 0031 must be rejected for all three reasons ──────────
check('the real plan 0031 TASKS.md is rejected (cycle + backward edge + symbol inversion)', () => {
  const md = readFileSync(join(repoRoot, 'docs/plans/0031-Sidebar-Unified-Plan-Task-Navigation/TASKS.md'), 'utf8')
  const res = analyzeTaskGraph(md)
  assert.ok(res.tasks.length >= 20, `expected ~24 parsed tasks, got ${res.tasks.length}`)
  assert.ok(res.findings.length > 0, 'plan 0031 must be REJECTED by the gate')
  // (a) the 2-cycle between the tab-content enum and the tab state.
  assert.ok(res.cycle, 'expected the add-tab-content-enum ↔ add-tab-state-to-app cycle')
  assert.ok(res.cycle.includes('add-tab-content-enum') && res.cycle.includes('add-tab-state-to-app'), `cycle should involve the tab tasks, got ${JSON.stringify(res.cycle)}`)
  // (b) add-normalizer-struct depends on add-normalize-system-prompt, declared later.
  assert.ok(res.findings.some(f => /file-order INVERSION/.test(f.note) && /add-normalizer-struct/.test(f.note) && /add-normalize-system-prompt/.test(f.note)), 'expected the add-normalizer-struct → add-normalize-system-prompt file-order inversion')
  // (c) add-normalizer-struct uses PLANNER_NORMALIZE_SYSTEM_PROMPT, defined later by add-normalize-system-prompt.
  assert.ok(res.findings.some(f => /symbol-order INVERSION/.test(f.note) && /PLANNER_NORMALIZE_SYSTEM_PROMPT/.test(f.note)), 'expected the PLANNER_NORMALIZE_SYSTEM_PROMPT symbol-order inversion')
  console.log(`    plan 0031: ${res.tasks.length} tasks, cycle ${res.cycle.join('→')}, ${res.findings.length} blocker(s)`)
})

console.log(`\nAll ${passed} checks passed.`)
