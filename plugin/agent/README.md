# Enzyme — Semantic Search for Agent Runtimes

Enzyme compiles a markdown vault into a concept graph with 8ms local queries. This repo packages enzyme as a plugin for [OpenClaw](https://docs.openclaw.ai) and [Hermes](https://github.com/esaradev/hermes-agent), with self-installing binary bootstrap.

For the core CLI and how enzyme works, see [jshph/enzyme](https://github.com/jshph/enzyme).

## OpenClaw

Clone into your workspace or managed skills directory:

```bash
git clone https://github.com/jshph/enzyme-skill.git ~/.openclaw/skills/enzyme
```

The SKILL.md loads with `always: true` — enzyme tools are available every session without explicit invocation. On first session, the agent detects whether enzyme is installed and bootstraps it automatically.

**What happens on install:**
1. Agent checks for `enzyme` on PATH. If missing, runs `install.sh` to download the binary + embedding model (~52 MB total).
2. Agent checks for `.enzyme/enzyme.db` in the vault. If missing, walks you through guided setup: scans vault structure, proposes entity focus, initializes the index.
3. Agent writes enzyme tool instructions into your AGENTS.md so future sessions have context without re-running setup.

After setup, every session automatically refreshes the index and injects relevant vault context before the first model call.

## Hermes

```bash
hermes plugins install jshph/enzyme-skill
```

The plugin registers five tools (`enzyme_petri`, `enzyme_catalyze`, `enzyme_refresh`, `enzyme_status`, `enzyme_init`) and two lifecycle hooks:

- **`on_session_start`** — bootstraps binary if missing, refreshes vault index
- **`pre_llm_call`** — runs `enzyme petri --query` with the user's message, injects relevant entities and catalysts as context

Tools are gated by `check_fn` — hidden from the model until the binary is installed.

## Any agent runtime

Install the CLI directly:

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

Then point your agent at `enzyme/SKILL.md` in this repo for the full workflow instructions, or add this to your agent's system prompt:

```
enzyme petri --query "user's question"   # get relevant vault context
enzyme catalyze "query"                  # semantic search by concept
enzyme refresh --quiet                   # re-index on session start
```

Use catalyst phrases from petri results to compose catalyze queries — they reach content the user's raw words won't find.

## What's in this repo

```
enzyme/
├── SKILL.md                # Skill instructions (OpenClaw, AgentSkills spec)
├── plugin.yaml             # Hermes plugin manifest
├── __init__.py             # Hermes entry point
├── schemas.py              # Tool schemas (LLM-facing)
├── tools.py                # Tool handlers (shell out to enzyme CLI)
├── hooks.py                # Lifecycle hooks (session start, pre-LLM)
├── setup.py                # Binary bootstrap logic
├── install.sh              # CLI installer (curl-pipe-bash)
├── bin/                    # Platform binaries (macOS arm64, Linux x86_64/arm64)
├── models/                 # Embedding model (~23 MB, bundled via Git LFS)
├── references/
│   ├── petri-guide.md      # How to present vault overview results
│   └── search-guide.md     # How to present search results
└── scripts/
    └── setup.sh            # Offline setup (uses bundled binaries)
```

Two install paths coexist:
- **Bundled** (`scripts/setup.sh`) — copies platform binary from `bin/`, no network needed. Used by AgentSkills-compatible runtimes.
- **Download** (`install.sh`) — fetches latest release from GitHub. Used by OpenClaw, Hermes, and manual installs. ~50KB repo without LFS.

## How enzyme works

Enzyme reads the tags, links, folders, and timestamps in your markdown and builds a concept graph. From that structure it generates **catalysts** — thematic questions that surface connections keyword search misses.

A query for "why we keep rewriting the auth layer" finds the ADR from six months ago and a retro note about scope creep — even though neither shares keywords with the query. The catalyst bridged them.

**Core tools:**

| Command | Purpose |
|---------|---------|
| `enzyme petri` | Vault overview: trending entities with catalysts and activity trends |
| `enzyme petri --query "..."` | Same, ranked by relevance to a query |
| `enzyme catalyze "query"` | Semantic search — returns excerpts, file paths, contributing catalysts |
| `enzyme init` | First-time setup (~10-30s depending on vault size) |
| `enzyme refresh` | Incremental re-index (fast, skips if unchanged) |

## Requirements

- macOS Apple Silicon or Linux (x86_64, aarch64)
- A folder of markdown files (Obsidian vaults, Readwise exports, any `.md` corpus)
- Catalyst generation uses [OpenRouter](https://openrouter.ai) free tier by default, or set `OPENAI_API_KEY`

## Links

- [enzyme.garden](https://enzyme.garden) — docs and setup guide
- [jshph/enzyme](https://github.com/jshph/enzyme) — CLI releases and Claude Code plugin
- [Discord](https://discord.gg/nhvsqtKjQd)

## License

MIT
