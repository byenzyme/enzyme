<div align="center">

# 🧬 Enzyme

**Don't let your agent get lost in your knowledge base.**
**Enzyme compiles 1,000+ documents into a concept graph in under 20 seconds. 8ms queries on device.**

[![Discord](https://img.shields.io/discord/1191288276536008745?label=Discord&logo=discord&style=flat-square)](https://discord.gg/nhvsqtKjQd)
[![License](https://img.shields.io/github/license/useenzyme/enzyme?style=flat-square)](LICENSE)
[![Release](https://img.shields.io/github/v/release/useenzyme/enzyme?style=flat-square)](https://github.com/useenzyme/enzyme/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/useenzyme/enzyme/total?style=flat-square&label=Downloads&color=6c757d)](https://github.com/useenzyme/enzyme/releases)

[Website](https://memory.enzyme.garden) · [Docs](https://memory.enzyme.garden/docs) · [Discord](https://discord.gg/nhvsqtKjQd) · [Getting Started](#install)

</div>

Enzyme reads a knowledge base — markdown files, Obsidian vaults, Readwise exports, any text corpus — and compiles it into a concept graph. The graph captures the cross-cutting themes in your material as **catalysts**: pre-computed questions that an agent can search through instead of grepping through your files.

No conversation history needed. No runtime reasoning. The expensive work happens once at init. After that, queries run locally in ~8ms on an on-device embedding model.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/useenzyme/enzyme/main/install.sh | bash
```

On macOS you can also use Homebrew:

```bash
brew install useenzyme/enzyme/enzyme-cli
```

App plugins are optional and installed separately inside Claude Code or Codex:

```bash
claude plugin marketplace add useenzyme/enzyme
claude plugin install enzyme
```

The repo includes marketplace metadata at the root and the actual plugin bundle under `plugins/enzyme/`. That nested layout is intentional: Claude Code and Codex marketplaces discover installable plugins from repository subdirectories, while the root files point each marketplace at the shared bundle.

### MCP server

If you prefer MCP over the plugin, Enzyme ships a stdio MCP server that works with any MCP-compatible client (Claude Desktop, Cursor, etc):

```bash
claude mcp add enzyme -- enzyme mcp
```

The MCP server exposes `init`, `petri`, `catalyze`, and `status` tools — you can initialize and explore your vault entirely from the client without running CLI commands separately.

## Quick start

For agent-guided setup, install the runtime instructions from your markdown vault:

```bash
cd /path/to/your/vault
enzyme install codex      # Codex / Pi / generic .agents
enzyme install claude     # Claude Code
```

Then ask your agent: "Use Enzyme to inspect and initialize this vault." The skill will scan the workspace, confirm the setup stance, persist `~/.enzyme/config.toml`, run `enzyme init`, and demonstrate `petri`/`catalyze`.

Terminal-only setup is also supported:

```bash
enzyme scan --write-config
$EDITOR ~/.enzyme/config.toml
enzyme init                # compiles concept graph — under 20s for 1k docs
```

## What it does

Enzyme reads the structure of your knowledge base — tags, links, folders, timestamps — and builds semantic clusters with temporal weight on every entity. From those clusters it generates **catalysts**: thematic questions that cut across your content and surface connections keyword search can't reach.

A search for "why we keep rewriting the auth layer" finds the ADR from six months ago, a retro note about scope creep, and a Readwise highlight on accidental complexity — even if none of those share keywords with the query.

### Core concepts

- **Entities** — the tags, links, and folders in your content. Each one becomes a semantic cluster.
- **Catalysts** — pre-computed themes Enzyme discovers across your material. Searching through catalysts connects content that keyword and vector search miss.
- **Petri** — the compiled index: what's trending, what entities exist, and what catalysts are anchored to each.
- **Target search** — search an unfamiliar corpus through your vault's catalysts with `enzyme catalyze "query" --target /path/to/other/repo`. Enzyme prepares the target automatically on first use.

### Example: petri output

`enzyme petri` renders a readable tree in an interactive terminal. When stdout is piped or captured, it emits JSON for tools like `jq`:

```bash
enzyme petri | jq '.entities[:2]'
```

```json
[
  {
    "name": "system-design",
    "type": "tag",
    "activity_trend": "active",
    "frequency_12m": 84,
    "catalysts": [
      {
        "text": "What does the commitment to simplicity cost when the pressure to ship keeps winning?",
        "context": "velocity vs craft in infrastructure",
        "era": "2024-Q3"
      },
      {
        "text": "Where does the analysis of user needs gather information that delays rather than clarifies the core value?",
        "context": "research as avoidance",
        "era": "2025-Q1"
      }
    ]
  },
  {
    "name": "working-with-others",
    "type": "tag",
    "activity_trend": "rising",
    "frequency_12m": 47,
    "catalysts": [
      {
        "text": "What assumptions about leadership are held by those who are good at building things?",
        "context": "craft vs delegation",
        "era": "2024-Q4"
      },
      {
        "text": "How does the goal of not depending on others shape the approach to collaboration?",
        "context": "independence vs team trust",
        "era": "2025-Q2"
      }
    ]
  }
]
```

Each entity has catalysts spanning different eras — questions that cut across months of writing. These are what the agent searches through, not your raw text.

### Example: catalyze query

```bash
enzyme catalyze "why we keep rewriting the auth layer"
```

```json
{
  "query": "why we keep rewriting the auth layer",
  "results": [
    {
      "file_path": "retros/2024-q3-platform-retro.md",
      "content": "scoped auth extraction as a two-week project for the third time. real blocker wasn't the token service — nobody wanted to own the session model. every proposal added a layer instead of removing one.",
      "similarity": 1.46
    },
    {
      "file_path": "adrs/007-auth-service-extraction.md",
      "content": "the monolith's session handling has become the bottleneck for every team shipping independently. chose separation of concerns over the coordination cost of a new service boundary.",
      "similarity": 1.24
    },
    {
      "file_path": "reading/highlights-accelerate.md",
      "content": "'Teams that can deploy independently are twice as likely to be in the high-performer category.' — we keep choosing the rewrite over the boundary.",
      "similarity": 1.13
    }
  ],
  "top_contributing_catalysts": [
    {
      "entity": "system-design",
      "text": "What does the commitment to simplicity cost when the pressure to ship keeps winning?",
      "relevance_score": 0.74
    }
  ]
}
```

The query matched no keywords in the retro or the ADR. The catalyst bridged them — the retro talked about "scope creep" and the ADR talked about "separation of concerns," but the underlying tension was the same.

### Why compile-time?

Most memory tools build understanding at runtime — they need conversation history before they know anything about your content. Enzyme works the other way: it extracts the conceptual structure from what already exists. The first agent conversation is as rich as the hundredth.

This matters when you're building on imported content (reading highlights, curated collections, research corpora). There's no cold start. The intelligence layer is ready from the moment the content is indexed.

## Requirements

- A folder of markdown files (Obsidian vaults, Readwise exports, any `.md` corpus)
- macOS (Apple Silicon or Intel) or Linux (x86_64 or aarch64)
- First vault init works out of the box via Enzyme's hosted bootstrap; login is used for refresh, publishing, account credits, and additional vaults. You can also bring your own OpenAI-compatible key with `--use-env-llm`.

## Links

- [memory.enzyme.garden](https://memory.enzyme.garden) — landing page
- [Docs](https://memory.enzyme.garden/docs) — how it works, catalysts, target search, for teams
- [Setup guide](https://memory.enzyme.garden/setup) — install and configure
