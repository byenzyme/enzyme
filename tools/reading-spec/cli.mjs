#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { compile } from './compiler.mjs';
const [file, flag = '--plan', ...extra] = process.argv.slice(2);
if (!file || extra.length || !['--plan', '--toml', '--instructions'].includes(flag)) {
  console.error('Usage: node cli.mjs FILE [--plan|--toml|--instructions]'); process.exit(2);
}
try {
  const result = compile(await readFile(file, 'utf8'));
  for (const d of result.diagnostics) console.error(`${file}:${d.line}:${d.column}: ${d.severity}: ${d.message}`);
  if (!result.ok) process.exit(1);
  if (flag === '--toml' && !result.configReady) { console.error('TOML export blocked. The plan includes unsupported or unresolved settings.'); process.exit(1); }
  console.log(flag === '--toml' ? result.configToml : flag === '--instructions' ? result.instructions : JSON.stringify(result.plan, null, 2));
} catch (error) { console.error(error.message); process.exit(1); }
