import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { compile, parse } from './compiler.mjs';
const wrap = body => `vault "/notes" { ${body} }`;
const example = name => readFileSync(new URL(`examples/${name}.enzyme`, import.meta.url), 'utf8');

test('all bundled examples compile with intentional capability boundaries', () => {
  for (const name of ['vault', 'memory', 'current-engine', 'overrides']) {
    const result = compile(example(name)); assert.equal(result.ok, true, JSON.stringify(result.diagnostics));
    assert.equal(result.configReady, ['memory', 'current-engine'].includes(name));
  }
});
test('global, vault, and reading settings resolve without mutating siblings', () => {
  const r = compile(`learning { sample across time favor recent periods question budget 40 }
    vault "/notes" { learning { question budget 30 }
      learn questions from folder "archive" about decisions { favor all periods equally question budget 20 }
      learn questions from folder "inbox" about reflection
    }`);
  assert.equal(r.ok, true);
  assert.deepEqual(r.plan.vaults[0].readings.map(x => x.learning), [
    { budget: 20, sample: 'across_time', favor: 'equal' }, { budget: 30, sample: 'across_time', favor: 'recent' },
  ]);
  assert.equal(r.configToml, null);
});
test('named profile values and inline definitions preserve seek, notice, and ask', () => {
  const r = compile(`let taste = profile { seek "choices" notice ["repetition"] ask ["What changed?"] }
    ${wrap('learn questions from folder "feedback" about taste learn questions from tag "craft" about profile { seek "quality" notice ["constraints"] }')}`);
  assert.equal(r.ok, true);
  assert.deepEqual(r.plan.vaults[0].readings[0].profile.definition.ask, ['What changed?']);
  assert.equal(r.plan.vaults[0].readings[1].profile.definition.seek, 'quality');
  assert.equal(r.configReady, false);
});
test('declared built-in name is custom, not silently replaced by the preset', () => {
  const r = compile(`profile decisions { seek "something else" notice ["other evidence"] } ${wrap('learn questions from tag "work" about decisions')}`);
  assert.equal(r.plan.vaults[0].readings[0].profile.kind, 'custom'); assert.equal(r.configToml, null);
});
test('valid supported settings compile to real TOML', () => {
  const r = compile(example('current-engine')); assert.equal(r.configReady, true);
  const check = spawnSync('python3', ['-c', 'import tomllib,sys,json; print(json.dumps(tomllib.loads(sys.stdin.read())))'], { input: r.configToml, encoding: 'utf8' });
  assert.equal(check.status, 0, check.stderr);
  const v = JSON.parse(check.stdout).vaults['/workspace/notes'];
  assert.equal(v.min_top_catalysts, 40); assert.equal(v.max_embedding_files, 2048);
  assert.deepEqual(v.entities[1], { 'folder:people': { profile: 'relational' } });
  assert.equal(v.entities.at(-1), '#roots');
});
test('TOML escaping preserves quotes, backslashes, and control characters', () => {
  const r = compile(wrap('learn questions from link "A\\\"B\\\\C\\tD" about auto'));
  const p = spawnSync('python3', ['-c', 'import tomllib,sys,json; print(json.dumps(tomllib.loads(sys.stdin.read())))'], { input: r.configToml, encoding: 'utf8' });
  assert.equal(p.status, 0, p.stderr);
  assert.equal(JSON.parse(p.stdout).vaults['/notes'].entities[0], '[[A"B\\C\tD]]');
});
test('unknown profiles fail rather than silently choosing a default', () => {
  const r = compile(wrap('learn questions from folder "people" about relationshps'));
  assert.equal(r.ok, false); assert.equal(r.configToml, null); assert.equal(r.instructions, '');
  assert.equal(r.diagnostics[0].code, 'unknown-profile');
});
test('explicit expansion remains proposed and only accepts folders', () => {
  const r = compile(wrap('learn questions from folder "people" including linked pages about relationships'));
  assert.equal(r.ok, true); assert.equal(r.configReady, false); assert.equal(r.plan.vaults[0].readings[0].includeLinkedPages, true);
  assert.equal(compile(wrap('learn questions from tag "people" including linked pages about relationships')).ok, false);
});
test('memory and retrieval compile to instructions, never executable hooks', () => {
  const r = compile(example('memory'));
  assert.match(r.instructions, /enzyme petri --query/); assert.match(r.instructions, /enzyme catalyze/);
  assert.match(r.instructions, /supporting evidence/); assert.match(r.instructions, /refresh --quiet/);
  assert.match(r.instructions, /not automatic hooks/); assert.doesNotMatch(r.configToml, /remember|append|observations\.md/);
  assert.ok(r.configToml.includes('log:observations'));
  assert.ok(!r.diagnostics.some(d => d.code === 'memory-source'));
});
test('vault retrieval overrides global guidance and preserves instruction order', () => {
  const r = compile(`when asked { "global" }
    vault "/one" { when asked { "before" retrieve passages through learned questions "after" } }
    vault "/two" {}`);
  assert.deepEqual(r.plan.vaults[0].retrieval.map(x => x.text ?? x.type), ['before', 'retrieve', 'after']);
  assert.equal(r.plan.vaults[1].retrieval[0].text, 'global');
});
test('duplicate and excluded readings are rejected', () => {
  for (const source of [
    wrap('learn questions from tag "x" about auto learn questions from tag "#x" about decisions'),
    wrap('leave out folders ["private"] learn questions from folder "private/people" about relationships'),
    'vault "/notes" {} vault "/notes" {}',
    'profile p { seek "a" notice ["b"] } profile p { seek "a" notice ["b"] } vault "/notes" {}',
  ]) assert.equal(compile(source).ok, false, source);
});
test('errors include exact locations', () => {
  const r = compile('vault "/notes" {\n  nope\n}');
  assert.equal(r.diagnostics[0].line, 2); assert.equal(r.diagnostics[0].column, 3);
});
test('comments and punctuation inside quoted prompts stay literal', () => {
  assert.throws(() => parse('/* not supported */'), /Unexpected character/);
});
test('quoted strings cannot masquerade as syntax', () => {
  for (const source of ['"vault" "/notes" {}', 'vault "/notes" { "learn" questions from folder "x" about auto }']) assert.equal(compile(source).ok, false);
  const r = compile('vault "/notes" { when asked { "// { retrieve passages }" } } // trailing comment');
  assert.equal(r.ok, true); assert.match(r.instructions, /\/\/ \{ retrieve passages \}/);
});
test('invalid, incomplete, or unsupported constructs fail closed', () => {
  for (const source of [
    '', 'vault "oops', 'vault "/notes" {', 'vault "/notes" {} surprise',
    wrap('question budget 0'), wrap('question budget 3.5'), wrap('question budget 40 question budget 50'),
    wrap('learn questions from folder "x" about profile { seek "x" }'),
    wrap('learn questions from tags [] about auto'), wrap('expire after 90 days'),
    wrap('remember in "x.md" when { "sometimes" }'),
    'learning { sample recent } vault "/notes" {}',
  ]) assert.equal(compile(source).ok, false, source);
});
test('CLI refuses unsupported TOML export with no misleading stdout', () => {
  const p = spawnSync(process.execPath, ['cli.mjs', 'examples/vault.enzyme', '--toml'], { cwd: new URL('.', import.meta.url), encoding: 'utf8' });
  assert.equal(p.status, 1); assert.equal(p.stdout, ''); assert.match(p.stderr, /export blocked/);
});
test('CLI emits instructions separately from config', () => {
  const p = spawnSync(process.execPath, ['cli.mjs', 'examples/memory.enzyme', '--instructions'], { cwd: new URL('.', import.meta.url), encoding: 'utf8' });
  assert.equal(p.status, 0); assert.match(p.stdout, /Remember in/);
});
test('sampling defaults cannot disappear when entity selection is automatic', () => {
  const r = compile('learning { sample across time } vault "/notes" {}');
  assert.equal(r.ok, true); assert.equal(r.configReady, false); assert.equal(r.configToml, null);
});
test('relative paths require resolution before export', () => {
  for (const path of ['~/notes', 'notes']) {
    const r = compile(`vault "${path}" { learn questions from folder "people" about relationships }`);
    assert.equal(r.configReady, false); assert.ok(r.diagnostics.some(d => d.code === 'path'));
  }
});
