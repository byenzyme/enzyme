/** Reading Spec v0.1: pure parser/compiler shared by the browser and CLI.
 * No evaluation, filesystem access, model calls, or writes are performed here.
 */
export const BUILTINS = Object.freeze({
  relationships: 'relational', decisions: 'decision_trace', resonance: 'resonance_trace',
  preferences: 'preference_evidence', reflection: 'reflective', tensions: 'tension_trace',
  operational: 'operational', relational: 'relational', decision_trace: 'decision_trace',
  resonance_trace: 'resonance_trace', preference_evidence: 'preference_evidence',
  reflective: 'reflective', tension_trace: 'tension_trace',
});
class SpecError extends Error {
  constructor(message, token) { super(message); this.token = token; }
}
export function tokenize(source) {
  const out = []; let i = 0, line = 1, column = 1;
  const advance = () => { const c = source[i++]; if (c === '\n') { line++; column = 1; } else column++; return c; };
  while (i < source.length) {
    if (/\s/.test(source[i])) { advance(); continue; }
    if (source.slice(i, i + 2) === '//') { while (i < source.length && source[i] !== '\n') advance(); continue; }
    const start = { offset: i, line, column };
    const c = source[i];
    if ('{}[]=,'.includes(c)) { out.push({ type: c, value: advance(), ...start }); continue; }
    if (c === '"') {
      let raw = advance(); let closed = false;
      while (i < source.length) {
        const char = advance(); raw += char;
        if (char === '\\' && i < source.length) raw += advance();
        else if (char === '"') { closed = true; break; }
      }
      if (!closed) throw new SpecError('Unclosed string. Add a closing double quote.', start);
      let value; try { value = JSON.parse(raw); } catch { throw new SpecError('Invalid string. Use JSON escapes such as \\n or \\".', start); }
      out.push({ type: 'string', value, ...start }); continue;
    }
    const match = source.slice(i).match(/^(?:[A-Za-z_][A-Za-z_0-9-]*|[0-9]+)/);
    if (!match) throw new SpecError(`Unexpected character ${JSON.stringify(c)}.`, start);
    const value = match[0]; for (let n = 0; n < value.length; n++) advance();
    out.push({ type: /^[0-9]/.test(value) ? 'number' : 'word', value, ...start });
  }
  out.push({ type: 'eof', value: '', offset: i, line, column }); return out;
}
export function parse(source) {
  const tokens = tokenize(source); let index = 0;
  const peek = () => tokens[index];
  const at = value => peek().value === value && peek().type !== 'string';
  const fail = message => { throw new SpecError(message, peek()); };
  const take = value => { if (!at(value)) fail(`Expected “${value}”, found ${peek().type === 'eof' ? 'end of program' : JSON.stringify(peek().value)}.`); return tokens[index++]; };
  const accept = value => at(value) ? tokens[index++] : null;
  const typed = type => { if (peek().type !== type) fail(`Expected ${type}, found ${JSON.stringify(peek().value)}.`); return tokens[index++].value; };
  const string = () => { const value = typed('string'); if (!value.trim()) fail('Empty names, paths, and instructions are not allowed.'); return value; };
  const list = () => { take('['); const values = []; while (!at(']')) { values.push(string()); if (!accept(',')) break; } take(']'); if (!values.length) fail('Provide at least one item.'); return values; };
  const integer = (min, max) => { const token = peek(); const value = Number(typed('number')); if (!Number.isSafeInteger(value) || value < min || value > max) throw new SpecError(`Expected an integer between ${min} and ${max}.`, token); return value; };
  const unique = (object, key, value, token) => { if (Object.hasOwn(object, key)) throw new SpecError(`“${key}” is already set in this scope.`, token); object[key] = value; };
  function learningStatement(settings) {
    const token = peek();
    if (accept('sample')) {
      take('across'); take('time'); unique(settings, 'sample', 'across_time', token);
    } else if (accept('favor')) {
      if (accept('recent')) { take('periods'); unique(settings, 'favor', 'recent', token); }
      else { take('all'); take('periods'); take('equally'); unique(settings, 'favor', 'equal', token); }
    } else if (accept('question')) {
      take('budget'); unique(settings, 'budget', integer(3, 10000), token);
    } else fail('Expected “sample across time”, “favor recent periods”, “favor all periods equally”, or “question budget N”.');
  }
  function learningBlock() { const result = {}; take('{'); while (!at('}')) learningStatement(result); take('}'); return result; }
  function profileBody() {
    take('{'); const result = {};
    while (!at('}')) {
      const token = peek();
      if (accept('seek')) unique(result, 'seek', string(), token);
      else if (accept('notice')) unique(result, 'notice', list(), token);
      else if (accept('ask')) unique(result, 'ask', list(), token);
      else fail('A profile contains “seek”, “notice”, and optional “ask”.');
    }
    take('}'); if (!result.seek || !result.notice) fail('A profile needs both “seek” and “notice”.'); return result;
  }
  function reading() {
    const token = take('learn'); take('questions'); take('from');
    const kind = typed('word');
    if (!['folder', 'folders', 'tag', 'tags', 'link', 'links', 'log', 'logs'].includes(kind)) fail('Read from folder, tag, link, or log (or their plural forms).');
    const plural = kind.endsWith('s'); const names = plural ? list() : [string()];
    const result = { type: 'reading', kind: plural ? kind.slice(0, -1) : kind, names, learning: {}, token };
    const include = () => { take('linked'); take('pages'); if (result.includeLinkedPages) fail('“including linked pages” is already set.'); result.includeLinkedPages = true; };
    if (accept('including')) include();
    take('about'); result.profile = accept('profile') ? { inline: profileBody() } : { name: typed('word') };
    if (at('{')) result.learning = learningBlock();
    while (['sample', 'favor', 'question', 'including'].some(at)) {
      if (accept('including')) include(); else learningStatement(result.learning);
    }
    if (result.includeLinkedPages && result.kind !== 'folder') throw new SpecError('Only folders can include linked pages.', token);
    return result;
  }
  function retrieval() {
    const token = take('when'); take('asked'); take('{'); const steps = [];
    while (!at('}')) {
      if (peek().type === 'string') { steps.push({ type: 'guidance', text: string() }); continue; }
      if (accept('retrieve')) {
        take('passages'); let acrossVault = false;
        if (accept('across')) { take('the'); take('vault'); acrossVault = true; }
        take('through'); take('learned'); take('questions'); steps.push({ type: 'retrieve', acrossVault });
      } else if (accept('answer')) { take('with'); take('sources'); steps.push({ type: 'answer' }); }
      else fail('Use quoted guidance, “retrieve passages through learned questions”, or “answer with sources”.');
    }
    take('}'); if (!steps.length) fail('“when asked” needs at least one instruction.'); return { type: 'retrieval', steps, token };
  }
  function memory() {
    const token = take('remember'); take('in'); const destination = string(); take('when'); take('{');
    const conditions = [], guidance = []; let append = false;
    while (!at('}')) {
      if (peek().type === 'string') conditions.push(string());
      else if (accept('append')) {
        if (append) fail('Use one append observation block per destination.');
        append = true; take('observation'); take('{'); while (!at('}')) guidance.push(string()); take('}');
      } else fail('Use quoted capture conditions and an “append observation” block.');
    }
    take('}'); if (!conditions.length || !guidance.length) fail('Remember needs a capture condition and observation guidance.');
    return { type: 'memory', destination, conditions, guidance, token };
  }
  function vault() {
    const token = take('vault'); const path = string(); take('{'); const result = { type: 'vault', path, learning: {}, readings: [], targets: [], memories: [], token };
    while (!at('}')) {
      const next = peek();
      if (at('learn')) result.readings.push(reading());
      else if (accept('learning')) {
        const block = learningBlock(); for (const [key, value] of Object.entries(block)) unique(result.learning, key, value, next);
      } else if (['sample', 'favor', 'question'].some(at)) learningStatement(result.learning);
      else if (accept('leave')) { take('out'); take('folders'); unique(result, 'exclusions', list(), next); }
      else if (accept('prepare')) {
        take('up'); take('to'); const count = integer(0, 10000000); take('documents'); take('per'); take('run');
        take('newest'); take('first'); unique(result, 'embeddingLimit', count, next);
      } else if (accept('project')) { take('questions'); take('into'); result.targets.push(string()); }
      else if (at('remember')) result.memories.push(memory());
      else if (at('when')) unique(result, 'retrieval', retrieval(), next);
      else fail('Expected a reading, learning settings, exclusions, document limit, target, memory rule, or “when asked”.');
    }
    take('}'); return result;
  }
  const program = { profiles: [], learning: {}, vaults: [] }; let globalLearning = false;
  while (peek().type !== 'eof') {
    const token = peek();
    if (accept('profile')) { const name = typed('word'); program.profiles.push({ name, ...profileBody(), token }); }
    else if (accept('let')) { const name = typed('word'); take('='); take('profile'); program.profiles.push({ name, ...profileBody(), token }); }
    else if (accept('learning')) {
      if (globalLearning) fail('Use one global learning block.'); globalLearning = true; program.learning = learningBlock();
    } else if (at('vault')) program.vaults.push(vault());
    else if (at('when')) unique(program, 'retrieval', retrieval(), token);
    else fail('Expected “profile”, “let”, “learning”, “vault”, or “when asked”.');
  }
  if (!program.vaults.length) fail('Add a vault block to compile a reading plan.'); return program;
}
const q = JSON.stringify;
const clean = value => JSON.parse(JSON.stringify(value, (key, val) => key === 'token' ? undefined : val));
const selector = (kind, name) => kind === 'tag' ? `#${name.replace(/^#/, '')}` : kind === 'link' ? `[[${name.replace(/^\[\[|\]\]$/g, '')}]]` : `${kind}:${name}`;
const md = value => value.replace(/[\r\n]+/g, ' ');

export function compile(source) {
  const diagnostics = [];
  const report = (severity, code, message, token, blocksConfig = false) => diagnostics.push({ severity, code, message, line: token?.line ?? 1, column: token?.column ?? 1, blocksConfig });
  let ast;
  try { ast = parse(source); } catch (error) {
    if (!(error instanceof SpecError)) throw error;
    report('error', 'syntax', error.message, error.token, true);
    return { ok: false, configReady: false, diagnostics, plan: null, configToml: null, instructions: '', trace: [] };
  }
  const profiles = new Map();
  for (const profile of ast.profiles) {
    if (profile.name === 'auto' || profiles.has(profile.name)) report('error', 'duplicate-profile', `Profile “${profile.name}” is reserved or already defined.`, profile.token, true);
    profiles.set(profile.name, profile);
  }
  const trace = [], vaultPaths = new Set(), sections = [], instructions = [];
  const retrievalInstructions = rule => {
    const lines = ['### When context is needed'];
    for (const step of rule.steps) {
      if (step.type === 'guidance') lines.push(`- ${md(step.text)}`);
      if (step.type === 'retrieve') lines.push(
        '- Run `enzyme petri --query` with the user’s request to find relevant learned questions.',
        '- Compose an `enzyme catalyze` query from the request and relevant question vocabulary. Retrieve source passages; learned questions are search handles, not answers or evidence.',
        '- Search across the active vault. The entity a question was learned from is not a hard boundary on the passages it may retrieve.');
      if (step.type === 'answer') lines.push('- Ground the answer in retrieved source passages and cite their files.');
    }
    return lines;
  };
  const plan = { version: '0.1', profiles: clean(ast.profiles), defaults: clean(ast.learning), vaults: [] };
  for (const vault of ast.vaults) {
    if (vaultPaths.has(vault.path)) report('error', 'duplicate-vault', `Vault “${vault.path}” is declared twice.`, vault.token, true);
    vaultPaths.add(vault.path);
    const defaults = { budget: 15, ...ast.learning, ...vault.learning };
    const resolved = { path: vault.path, learning: defaults, exclusions: vault.exclusions ?? [], embeddingLimit: vault.embeddingLimit ?? 1024, targets: [...new Set(vault.targets)], readings: [], memories: clean(vault.memories) };
    const entities = [], seen = new Set();
    const header = `[vaults.${q(vault.path)}]`;
    const settings = [header];
    if (vault.exclusions) settings.push(`excluded_folders = ${q(vault.exclusions)}`);
    if (vault.embeddingLimit !== undefined) settings.push(`max_embedding_files = ${vault.embeddingLimit}`);
    settings.push(`min_top_catalysts = ${defaults.budget}`);
    if (resolved.targets.length) settings.push(`targets = ${q(resolved.targets)}`);
    if (!vault.path.startsWith('/') && !/^[A-Za-z]:[\\/]/.test(vault.path)) report('warning', 'path', 'Use an absolute vault path before installing this config; vault keys identify exact locations.', vault.token, true);
    if (!vault.readings.length && (defaults.sample || defaults.favor)) report('warning', 'sampling', 'Sampling/recency defaults are preserved in the plan but are not current TOML controls, including for automatically selected entities.', vault.token, true);
    trace.push({ line: vault.token.line, title: vault.path, detail: `Question budget ${defaults.budget} for the largest entity; proportional coverage with a floor of 3. Embedding limit ${resolved.embeddingLimit === 0 ? 'unlimited' : resolved.embeddingLimit + ', newest first'} per run. These are separate budgets.` });
    instructions.push(`# Reading instructions: ${md(vault.path)}`, '', 'Run Enzyme from this vault. These are agent instructions, not automatic hooks or a background watcher.', '');
    for (const reading of vault.readings) {
      const effective = { ...defaults, ...reading.learning };
      const ref = reading.profile.name;
      const custom = reading.profile.inline ?? profiles.get(ref);
      const builtin = custom ? null : (ref === 'auto' ? null : BUILTINS[ref]);
      if (!custom && ref !== 'auto' && !builtin) report('error', 'unknown-profile', `Unknown profile “${ref}”. Define it or use a built-in such as relationships, decisions, or resonance.`, reading.token, true);
      if (custom) report('warning', 'custom-profile', 'Custom seek/notice/ask profiles are preserved in the plan. The current runtime accepts built-in profile keys only; TOML export is blocked rather than substituting a different profile.', reading.token, true);
      if (effective.sample || effective.favor) report('warning', 'sampling', `Sampling/recency policy (${effective.sample ?? 'engine default'}, ${effective.favor ?? 'engine default'}) is preserved in the plan but is not a current TOML control. It does not implement expiry.`, reading.token, true);
      if (effective.budget !== defaults.budget) report('warning', 'local-budget', 'Per-reading question budgets are preserved in the plan; current TOML only supports a vault-wide budget.', reading.token, true);
      if (reading.includeLinkedPages) report('warning', 'expansion', 'Linked-page expansion is currently detected automatically. The engine does not enforce this switch or guarantee children inherit the folder profile.', reading.token, true);
      for (const name of reading.names) {
        const entity = selector(reading.kind, name);
        if (seen.has(entity)) report('error', 'duplicate-reading', `“${entity}” has multiple readings. The current engine assigns one profile per entity.`, reading.token, true);
        seen.add(entity);
        if (reading.kind === 'folder' && resolved.exclusions.some(excluded => name === excluded || name.startsWith(excluded + '/'))) report('error', 'excluded-reading', `“${name}” is both excluded and selected for learning.`, reading.token, true);
        resolved.readings.push({ entity, profile: custom ? { kind: 'custom', name: ref ?? null, definition: clean(custom) } : { kind: ref === 'auto' ? 'auto' : 'builtin', key: builtin ?? null }, learning: effective, includeLinkedPages: !!reading.includeLinkedPages });
        entities.push(custom || !builtin ? q(entity) : `{ ${q(entity)} = { profile = ${q(builtin)} } }`);
        trace.push({ line: reading.token.line, title: entity, detail: `${custom ? custom.seek : ref === 'auto' ? 'Assign a profile automatically' : `Use ${builtin}`}. Budget ${effective.budget}${reading.includeLinkedPages ? '; include linked pages (proposed)' : ''}.`, learning: effective });
      }
    }
    settings.push(`entities = [\n${entities.map(e => '  ' + e).join(',\n')}\n]`); sections.push(settings.join('\n'));
    const retrieval = vault.retrieval ?? ast.retrieval;
    if (retrieval) { resolved.retrieval = clean(retrieval.steps); instructions.push(...retrievalInstructions(retrieval), ''); }
    else instructions.push('No retrieval policy was declared. Keep the installed Enzyme runtime skill’s retrieval behavior.', '');
    for (const memory of vault.memories) {
      instructions.push(`### Remember in ${q(md(memory.destination))}`, 'Consider appending an observation when any of these conditions is supported by the interaction:', ...memory.conditions.map(c => `- ${md(c)}`), 'When writing:', ...memory.guidance.map(g => `- ${md(g)}`),
        '- Follow host permissions and the user’s current instructions. This declaration does not execute a write or install a trigger.',
        '- Append a dated observation without replacing existing content; preserve supporting sources and context.',
        '- After writing, run `enzyme refresh --quiet` so the new material can enter retrieval. Follow the installed runtime’s provider and refresh requirements.', '');
      const stem = memory.destination.split('/').at(-1).replace(/\.md$/i, '').toLowerCase();
      const fed = resolved.readings.some(r => r.entity.toLowerCase() === `log:${stem}` || (r.entity.startsWith('folder:') && memory.destination.startsWith(r.entity.slice(7) + '/')));
      if (!fed) report('warning', 'memory-source', `“${memory.destination}” has a write rule but no matching log/folder reading. Add one if observations should develop their own questions.`, memory.token);
      trace.push({ line: memory.token.line, title: `Remember → ${memory.destination}`, detail: `${memory.conditions.length} capture conditions compile to agent guidance. No files are written by this playground.` });
    }
    for (const target of resolved.targets) instructions.push(`External target: ${q(md(target))}. When relevant, use enzyme catalyze with --target. The vault’s questions are the lens; target-only themes may be missed.`);
    if (!vault.readings.length) report('warning', 'automatic-selection', 'No readings are declared: an empty entities list leaves entity selection to the engine.', vault.token);
    plan.vaults.push(resolved);
  }
  const ok = !diagnostics.some(d => d.severity === 'error');
  const configReady = ok && !diagnostics.some(d => d.blocksConfig);
  return { ok, configReady, diagnostics, plan, configToml: configReady ? '# Compiled by Enzyme Reading Spec v0.1\n# Review paths before merging into ~/.enzyme/config.toml.\n\n' + sections.join('\n\n') + '\n' : null, instructions: ok ? instructions.join('\n') : '', trace };
}
