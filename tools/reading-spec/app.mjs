// Parsing and compilation live in enzyme-spec (Rust).
const $ = id => document.getElementById(id);
const editor = $('source');
let compileVersion = 0;
let result, tab = 'reading', timer, loadVersion = 0;
const drafts = new Map();
let activeExample = 'vault';
const escapeHtml = value => String(value).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const el = (name, text, className) => { const node = document.createElement(name); if (text !== undefined) node.textContent = text; if (className) node.className = className; return node; };
function highlight() {
  const pattern = /\/\/[^\n]*|"(?:\\.|[^"\\])*"|\b(?:profile|let|vault|learning|seek|notice|ask|learn|questions|from|folder|folders|tag|tags|log|link|about|including|linked|pages|select|by|frequency|recency|up|to|sample|across|time|favor|recent|periods|question|budget|when|asked|retrieve|passages|through|learned|answer|with|sources|remember|in|append|observation|project|into|prepare|newest|first|leave|out|auto)\b|\b\d+\b/g;
  let html = '', last = 0;
  for (const match of editor.value.matchAll(pattern)) {
    html += escapeHtml(editor.value.slice(last, match.index));
    const cls = match[0].startsWith('//') ? 'comment' : match[0].startsWith('"') ? 'string' : /^\d/.test(match[0]) ? 'number' : 'keyword';
    html += `<span class="${cls}">${escapeHtml(match[0])}</span>`; last = match.index + match[0].length;
  }
  $('highlight').innerHTML = html + escapeHtml(editor.value.slice(last)) + '\n'; syncScroll();
}
function syncScroll() { $('highlight').scrollTop = editor.scrollTop; $('highlight').scrollLeft = editor.scrollLeft; }
function cursor() { const before = editor.value.slice(0, editor.selectionStart).split('\n'); $('position').textContent = `Ln ${before.length}, Col ${before.at(-1).length + 1}`; }
function jump(line, column = 1) {
  const lines = editor.value.split('\n'); const offset = lines.slice(0, line - 1).reduce((sum, text) => sum + text.length + 1, 0) + column - 1;
  editor.focus(); editor.setSelectionRange(offset, offset); editor.scrollTop = Math.max(0, (line - 5) * parseFloat(getComputedStyle(editor).lineHeight)); syncScroll(); cursor();
}
function sourceButton(line, column) { const button = el('button', `line ${line}`, 'source-link'); button.onclick = () => jump(line, column); return button; }
function diagnosticView() {
  const container = $('diagnostics'); container.replaceChildren();
  const unique = result.diagnostics.filter((d, i, all) => all.findIndex(x => x.code === d.code && x.message === d.message) === i);
  $('diagnostic-count').textContent = unique.length ? `${unique.length} ${unique.length === 1 ? 'note' : 'notes'}` : 'All clear';
  for (const d of unique) {
    const row = el('div', undefined, `diagnostic ${d.severity}`); row.append(el('strong', d.severity === 'error' ? 'ERROR' : d.code === 'path' ? 'PATH' : d.blocksConfig ? 'PROPOSED' : 'NOTE'), sourceButton(d.line, d.column), el('span', d.message)); container.append(row);
  }
  if (!unique.length) container.append(el('div', 'Compiled by the Rust runtime parser. Config drives init/refresh; agent policies compile separately to instructions.', 'diagnostic'));
}
function badge(text, proposed = false) { return el('span', text, `badge${proposed ? ' proposed' : ''}`); }
function renderReading(container) {
  container.append(el('h2', 'A reading you can inspect.', 'result-intro'), el('p', 'Profiles shape questions. Questions lead back to source passages.', 'result-note'));
  for (const vault of result.plan.vaults) {
    const policy = el('div', undefined, 'policy'); policy.append(badge(vault.path), badge(`budget ${vault.learning.budget}`)); container.append(policy);
    for (const reading of vault.readings) {
      const card = el('article', undefined, 'reading-card'); const heading = el('h3', reading.entity);
      const trace = result.trace.find(t => t.title === reading.entity); if (trace) heading.append(sourceButton(trace.line)); card.append(heading);
      const profile = reading.profile;
      card.append(el('p', profile.kind === 'custom' ? profile.definition.seek : profile.kind === 'auto' ? 'Choose a profile automatically from this material.' : `Read through the ${profile.key.replaceAll('_', ' ')} profile.`));
      if (profile.kind === 'custom') {
        const list = el('ul'); for (const text of profile.definition.notice) list.append(el('li', text)); card.append(list);
      }
      card.append(badge(profile.kind === 'custom' ? profile.name ?? 'inline profile' : profile.key ?? 'auto', false));
      if (reading.includeLinkedPages) card.append(badge('including linked pages'));
      const selection = reading.learning.selection;
      if (selection && reading.entity.startsWith('folder:')) {
        const amount = selection.amount;
        const cap = amount.up_to == null ? '' : ` up to ${amount.up_to}`;
        card.append(badge(`select ${amount.kind === 'percent' ? `${amount.percent}%` : amount.count} by ${selection.by}${cap}`));
      }
      if (reading.learning.sampling_mode) card.append(badge(`sample by ${reading.learning.sampling_mode}`));
      card.append(badge(`base budget ${reading.learning.budget}`)); container.append(card);
    }
    for (const memory of vault.memories) {
      const card = el('article', undefined, 'reading-card'); card.append(el('h3', `Remember → ${memory.destination}`));
      for (const condition of memory.conditions) card.append(el('p', condition)); card.append(badge('agent instructions')); container.append(card);
    }
    if (vault.retrieval) {
      const card = el('article', undefined, 'reading-card'); card.append(el('h3', 'When asked'));
      for (const step of vault.retrieval) card.append(el('p', step.type === 'guidance' ? step.text : step.type === 'retrieve' ? 'Request → relevant learned questions → source passages' : 'Answer with sources.'));
      container.append(card);
    }
    for (const target of vault.targets) { const card = el('article', undefined, 'reading-card'); card.append(el('h3', `Project questions → ${target}`), el('p', 'Search this material through the questions developed from the vault.')); container.append(card); }
  }
}
function render() {
  const container = $('result'); const scroll = container.scrollTop; container.replaceChildren();
  $('status').textContent = result.ok ? result.configReady ? 'Compiled · Rust' : 'Compiled · preview' : 'Check syntax';
  $('status').className = result.ok ? '' : 'error';
  const names = { reading: ['Resolved reading plan', 'Download plan ↓'], plan: ['JSON · complete semantics', 'Download plan ↓'], config: ['.enzyme · runtime configuration', 'Download .enzyme ↓'], instructions: ['Markdown · agent guidance', 'Download instructions ↓'] };
  $('output-label').textContent = names[tab][0]; $('download').textContent = names[tab][1];
  $('download').disabled = !result.ok || (tab === 'config' && !result.configReady);
  if (!result.ok) {
    const box = el('div', undefined, 'empty'); box.append(el('h3', 'A small correction first.'), el('p', result.diagnostics.find(d => d.severity === 'error').message)); container.append(box);
  } else if (tab === 'reading') renderReading(container);
  else if (tab === 'config' && !result.configReady) {
    const box = el('div', undefined, 'empty'); box.append(el('h3', 'This reading goes beyond today’s config.'), el('p', 'The complete plan preserves your intent. TOML export is withheld because it would discard or change part of that intent.'), el('p', 'Try “Current engine” for an exportable program, or inspect the compiler notes below.')); container.append(box);
  } else container.append(el('pre', tab === 'plan' ? JSON.stringify(result.runtimePlan ?? result.plan, null, 2) : tab === 'config' ? result.configSource : result.instructions, 'code-output'));
  container.scrollTop = scroll; diagnosticView();
}
async function run() {
  clearTimeout(timer); const version = ++compileVersion;
  $('status').textContent = 'Compiling…';
  try {
    const response = await fetch('/api/compile', { method: 'POST', headers: { 'Content-Type': 'text/plain' }, body: editor.value });
    if (!response.ok) throw new Error(`Compiler returned ${response.status}`);
    const next = await response.json(); if (version !== compileVersion) return;
    result = next; render();
  } catch (error) { if (version === compileVersion) { $('status').textContent = error.message; $('download').disabled = true; } }
}
async function loadExample(name) {
  ++compileVersion; clearTimeout(timer);
  $('status').textContent = 'Loading example…';
  $('download').disabled = true;
  const version = ++loadVersion; drafts.set(activeExample, editor.value); activeExample = name;
  try {
    let source = drafts.get(name);
    if (source === undefined) { const response = await fetch(`examples/${name}.enzyme`); if (!response.ok) throw new Error('Could not load example'); source = await response.text(); }
    if (version !== loadVersion) return; editor.value = source; editor.setSelectionRange(0, 0); editor.scrollTop = 0; highlight(); cursor(); run();
  } catch (error) { $('status').textContent = error.message; }
}
editor.addEventListener('input', () => { highlight(); cursor(); clearTimeout(timer); timer = setTimeout(run, 180); });
editor.addEventListener('scroll', syncScroll); editor.addEventListener('click', cursor); editor.addEventListener('keyup', cursor);
editor.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); run(); }
  if (event.key === 'Tab') { event.preventDefault(); editor.setRangeText('  ', editor.selectionStart, editor.selectionEnd, 'end'); editor.dispatchEvent(new Event('input')); }
});
$('compile').onclick = run; $('example').onchange = event => loadExample(event.target.value);
const tabs = [...document.querySelectorAll('[data-tab]')];
function selectTab(button) {
  if (!result) return;
  tab = button.dataset.tab;
  for (const candidate of tabs) { candidate.setAttribute('aria-selected', String(candidate === button)); candidate.tabIndex = candidate === button ? 0 : -1; }
  $('result').setAttribute('aria-labelledby', button.id); render();
}
for (const button of tabs) {
  button.onclick = () => selectTab(button);
  button.onkeydown = event => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault(); const index = tabs.indexOf(button);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
    tabs[next].focus(); selectTab(tabs[next]);
  };
}
$('download').onclick = () => {
  const ext = tab === 'config' ? 'enzyme' : tab === 'instructions' ? 'md' : 'json';
  const content = ext === 'enzyme' ? result.configSource : ext === 'md' ? result.instructions : JSON.stringify(result.runtimePlan ?? result.plan, null, 2);
  const url = URL.createObjectURL(new Blob([content], { type: 'text/plain;charset=utf-8' })); const link = el('a'); link.href = url; link.download = `enzyme-reading.${ext}`; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
};
// Start without overwriting the initial example with an empty draft.
activeExample = ''; loadExample('vault');
