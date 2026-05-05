# Enzyme — Compile-time semantic memory for agent workspaces

Enzyme is a Hermes plugin and OpenClaw skill that gives agents structural memory over Obsidian vaults, markdown knowledge bases, and any accumulated document corpus. It compiles your workspace into a concept graph with 8ms local semantic search — before the conversation starts.

Every agent session starts from scratch. RAG embeds and retrieves per query. Memory tools store conversation facts and run LLM inference to reason over them. Both cost tokens at query time. Both require tool calls the model has to decide to make, frame correctly, and wait for. Both start empty on day one.

Enzyme is a compile step. It reads your workspace's tags, links, folders, and timestamps, generates thematic questions (catalysts) from the full timeline, and pre-computes every relationship. At query time: a database lookup. No LLM call. No API call. ~8ms on device.

```
> "Why do we keep rewriting the auth layer?"

  → reaches across the ADR from six months ago, a retro note about scope creep,
    and a journal entry connecting them — none share keywords with the query.
    A catalyst bridged them.
```

One `enzyme petri` call at session start gives the model your trending topics, thematic tensions, and a search vocabulary that reaches content the user's raw words would never match — in 80% fewer tokens than brute-force agent search. The agent arrives knowing the room.

11 MB binary. 23 MB embedding model. Compiles 1,000+ docs in under 20s. $0 per query.

This repo packages enzyme as a plugin for [Hermes](https://github.com/NousResearch/hermes-agent) and [OpenClaw](https://docs.openclaw.ai), with self-installing binary bootstrap. For the core CLI: [jshph/enzyme](https://github.com/jshph/enzyme). Docs and architecture: [enzyme.garden](https://enzyme.garden).

---

## Hermes plugin — Install and quick start

### Install the plugin

```bash
hermes plugins install jshph/enzyme-skill
```

Verify it loaded:

```bash
hermes plugins list
```

You should see:

```
enzyme │ enabled │ 0.x.x │ Vault intelligence — concept graph and semantic search for markdown
```

### First session

Start Hermes in your vault directory:

```bash
cd ~/your-vault
hermes
```

On your first session, enzyme will:

1. **Bootstrap the binary** if `enzyme` isn't on PATH (~38 MB download, takes 10-30s)
2. **Refresh the index** to catch any vault changes since last session
3. **Register tools** — five tools become available to the model

Just start talking. The model reaches for enzyme tools when it needs vault context — you don't have to ask for it.

Here's what that looks like in practice. Three turns, no explicit tool requests:

```
You:     What's been active across my notes lately?
Hermes:  [calls enzyme_petri] Three threads are trending...
         positioning (messaging keeps shifting — "tool" → "layer" → "engine"),
         Q1-retro (two team leads flagged the same onboarding bottleneck
         independently), customer-research (three interviews last week all
         circled back to trust)...

You:     What's the throughline in those customer interviews?
Hermes:  [calls enzyme_catalyze "customer interviews trust recurring theme"]
         [reads notes/2026-03-25-sarah-call.md, notes/2026-03-27-marcus-call.md]
         All three conversations started on pricing but ended on trust.
         Sarah said "I need to know it won't break mid-quarter." Marcus
         described evaluating tools as "trying to figure out who's still
         going to be here in a year." The word "trust" doesn't appear in
         any of the transcripts — but the concern runs through all of them...

You:     Did we land on final messaging for the launch?
Hermes:  [calls enzyme_catalyze "launch messaging positioning decided"]
         [greps for "final", "approved", "go with"]
         Not yet. The March 20 strategy doc proposed "compile-time memory"
         as the anchor. The March 28 meeting notes show the team liked it
         but wanted a customer-facing version. No follow-up after that...
```

The model composed its own search queries, combined enzyme with grep, and read the actual files — all without being told which tools to use or how to formulate queries.

### How the plugin works

The plugin registers three lifecycle hooks and five tools.

**Hooks** run automatically:

| Hook | When | What it does |
|------|------|--------------|
| `on_session_start` | Session begins | Bootstraps binary if missing, runs `enzyme refresh` |
| `pre_llm_call` | First user message | Runs `enzyme petri --query` with the user's message, injects relevant entities and catalysts as ephemeral context |
| `on_session_end` | Session closes normally | Runs `enzyme refresh` to index notes written during the session |

The `pre_llm_call` hook fires on the first turn to seed the model with vault context. After that, the model uses enzyme tools explicitly when it needs to go deeper — it doesn't run petri on every turn.

**Tools** are available for the model to call as needed:

| Tool | What it does |
|------|--------------|
| `enzyme_petri` | Vault overview — trending entities with catalysts and activity trends. Open-ended (no query) for a full picture, or pass a query to rank by relevance. |
| `enzyme_catalyze` | Semantic search — returns note excerpts, file paths, and the catalysts that bridged the match. |
| `enzyme_refresh` | Incremental re-index. Fast (~100ms) when nothing changed. |
| `enzyme_status` | Document count, entity count, embedding coverage, API key status. |
| `enzyme_init` | First-time vault setup. Builds the concept graph, generates catalysts, creates embeddings. |

Tools are gated by `check_fn` — hidden from the model until the enzyme binary is installed and on PATH.

### Optional: Set an API key for catalysts

Enzyme generates catalysts using an LLM. Without an API key, everything works except catalyst regeneration.

Set one of these in your `~/.hermes/.env`:

```bash
OPENROUTER_API_KEY=your-key    # Free tier works
# or
OPENAI_API_KEY=your-key
```

### Update the plugin

```bash
cd /tmp && git clone --depth 1 https://github.com/jshph/enzyme-skill.git enzyme-update \
  && rm -rf ~/.hermes/plugins/enzyme \
  && mv enzyme-update ~/.hermes/plugins/enzyme \
  && rm -rf enzyme-update
```

Or if you want to track upstream for easier updates:

```bash
git clone https://github.com/jshph/enzyme-skill.git ~/.hermes/enzyme-skill-repo
ln -sfn ~/.hermes/enzyme-skill-repo ~/.hermes/plugins/enzyme
```

Then update with `cd ~/.hermes/enzyme-skill-repo && git pull`.

---

## OpenClaw skill

> **Status:** The OpenClaw integration is designed and documented but hasn't been end-to-end tested yet. The SKILL.md metadata follows the OpenClaw spec, and the install block and skill instructions should work — but edge cases around `kind: "download"` install, `always: true` token cost, and skill directory structure after clone haven't been verified. If you hit issues, please [open an issue](https://github.com/jshph/enzyme/issues) or find us on [Discord](https://discord.gg/nhvsqtKjQd).

### Install the skill

```bash
git clone https://github.com/jshph/enzyme-skill.git ~/.openclaw/skills/enzyme
```

The SKILL.md loads with `always: true` — enzyme is available every session without explicit invocation. On first session, the agent:

1. **Detects `anyBins: ["enzyme"]`** requirement and bootstraps the binary if missing
2. **Walks you through vault setup** following the setup-guide
3. **Writes enzyme instructions** into your AGENTS.md for persistence across sessions

### First session

Start OpenClaw in your vault directory:

```bash
cd ~/your-vault
openclaw
```

The agent should run `enzyme refresh` at session start and reach for `enzyme petri` on your first message. The same kinds of prompts work:

- *"What's been active across my notes lately?"*
- *"What's the throughline in those customer interviews?"*
- *"Did we land on final messaging for the launch?"*

### How the skill works

The SKILL.md gives the agent instructions for the full session lifecycle:

| Phase | What the agent does |
|-------|---------------------|
| Session start | Runs `enzyme refresh --quiet` to catch vault changes |
| First message | Runs `enzyme petri` — open-ended if the prompt is broad, or `--query "..."` if the user has a specific direction |
| Deeper search | Uses `enzyme catalyze` with catalyst vocabulary from petri to reach content the user's words wouldn't match |
| Writing notes | Follows the note template with existing tags and wikilinks |

Unlike Hermes, OpenClaw doesn't have lifecycle hooks — context injection and refresh are driven by skill instructions rather than automated callbacks. The agent decides when to call enzyme. Petri runs once at the start to seed context, not on every turn.

The same organic behavior applies — the agent composes its own enzyme queries, combines them with file reads and grep, and surfaces vault content without you specifying tools.

### Recommended configuration

After installing the skill, add these to your `openclaw.json`:

**Point memory search at your vault** so OpenClaw's native search also reaches vault content:

```json5
memorySearch: {
  extraPaths: ["/path/to/your/vault"]
}
```

**Add enzyme refresh to your heartbeat** so the index stays current between sessions. In your `HEARTBEAT.md`:

```markdown
- [ ] Run `enzyme refresh --quiet` to catch vault changes
```

**Schedule a daily refresh** for vaults with external content sources (Readwise syncs, channel captures):

```json5
cron: {
  enabled: true
}
```

Then tell your agent: *"Create a daily cron job that runs `enzyme refresh --quiet` at 9 AM."*

**Set your API key** for catalyst generation (optional — enzyme works without it, but catalysts won't regenerate):

```json5
skills: {
  entries: {
    enzyme: {
      env: {
        OPENROUTER_API_KEY: "your-key"
      }
    }
  }
}
```

Or set `OPENAI_API_KEY` / `OPENROUTER_API_KEY` in your environment. OpenRouter's free tier works.

### Update the skill

```bash
cd ~/.openclaw/skills/enzyme && git pull
```

### Open questions

These are untested areas we're tracking:

- **`kind: "download"` installer** — does OpenClaw run the install.sh script correctly, or does it expect an archive? No existing skill uses `kind: "download"`.
- **`always: true` token cost** — does the full SKILL.md load into every turn's context, or just make the skill available on demand?
- **Skill directory structure** — after `git clone`, does OpenClaw find `SKILL.md` at the repo root correctly?

---

## Any agent runtime

Install the CLI directly:

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

Then point your agent at `SKILL.md` for the full workflow, or add this to your agent's system prompt:

```
On session start, run `enzyme refresh --quiet`.
On the user's first message, run `enzyme petri --query "their message"` for vault context.
Use catalyst phrases from petri to compose `enzyme catalyze` queries.
```

---

## How compile-time semantic search works

Enzyme is a compile step. The expensive work happens once at init — entity extraction, catalyst generation, similarity pre-computation — and everything after that is fast lookups.

**1. Structure reading** (~2s). Walks the corpus and extracts entities — tags, links, folders — with temporal metadata. When each entity appeared, when it was last active, whether it's accelerating or going dormant.

**2. Embedding** (~5s). Documents split into chunks and embedded with a local ONNX model. No API call. No data leaving the machine.

**3. Catalyst generation** (~10s). For each entity, the engine samples context across temporal eras and generates catalysts — thematic questions that probe what the content is actually about. A catalyst for an entity spanning 18 months doesn't ask "what happened?" It asks something like: *"The team revisited caching three times — once as a performance fix, once as a cost concern, once as a reliability question. What changed between each return?"*

**4. Similarity pre-computation** (~3s). Every catalyst compared against every chunk. Top matches stored. This is what makes queries fast — at query time, there's no vector search. The similarities are already computed.

Catalysts regenerate as the workspace grows. The same entity produces different catalysts six months from now. The questions stay alive where summaries go stale. When the agent writes a note back into the workspace, it enters the same index — the loop compounds.

| Command | Purpose |
|---------|---------|
| `enzyme petri` | Vault overview: trending entities with catalysts and activity trends |
| `enzyme petri --query "..."` | Same, ranked by relevance to a query |
| `enzyme catalyze "query"` | Semantic search — returns excerpts, file paths, contributing catalysts |
| `enzyme init` | First-time setup (~10-30s depending on vault size) |
| `enzyme refresh` | Incremental re-index (fast, skips if unchanged) |

## What's in this repo

```
SKILL.md                    # Skill instructions (OpenClaw, AgentSkills spec)
plugin.yaml                 # Hermes plugin manifest
__init__.py                 # Hermes entry point
schemas.py                  # Tool schemas (LLM-facing)
tools.py                    # Tool handlers (shell out to enzyme CLI)
hooks.py                    # Lifecycle hooks (session start, pre-LLM, session end)
install.sh                  # CLI installer (curl-pipe-bash)
bin/                        # Platform binaries (macOS arm64, Linux x86_64/arm64)
```

Install path:
- **Download** (`install.sh`) — fetches latest release from GitHub

## Requirements

- macOS Apple Silicon or Linux (x86_64, aarch64)
- A folder of markdown files (Obsidian vaults, Readwise exports, any `.md` corpus)
- Catalyst generation uses [OpenRouter](https://openrouter.ai) free tier by default, or set `OPENAI_API_KEY`

## Testing

From `enzyme-rust`, run the deterministic plugin smoke test:

```bash
python3 scripts/test-hermes-plugin-smoke.py
```

That verifies the Hermes plugin layout, imports, registered hooks, and registered tools without invoking Hermes or installing binaries.

For a live Hermes integration run, use an initialized vault with Hermes configured:

```bash
cd /path/to/vault
python /path/to/enzyme-rust/scripts/test-hermes-e2e.py
```

The live test exercises real `hermes chat` turns, plugin hooks, and enzyme tool calls, so it requires `hermes`, an `.enzyme/enzyme.db`, and a working model provider.

## Links

- [enzyme.garden](https://enzyme.garden) — docs and setup guide
- [jshph/enzyme](https://github.com/jshph/enzyme) — CLI releases and Claude Code plugin
- [Discord](https://discord.gg/nhvsqtKjQd)

## License

MIT
