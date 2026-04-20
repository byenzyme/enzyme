<div align="center">

# Enzyme

**The connections in your content, surfaced before the conversation starts.**

Enzyme compiles a folder of documents into a concept graph. 1,000+ docs in under 15 seconds. 8ms queries on device.

[![Discord](https://img.shields.io/discord/1191288276536008745?label=Discord&logo=discord&style=flat-square)](https://discord.gg/nhvsqtKjQd)
[![License](https://img.shields.io/github/license/jshph/enzyme?style=flat-square)](LICENSE)
[![Release](https://img.shields.io/github/v/release/jshph/enzyme?style=flat-square)](https://github.com/jshph/enzyme/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/jshph/enzyme/total?style=flat-square&label=Downloads&color=6c757d)](https://github.com/jshph/enzyme/releases)

[Website](https://enzyme.garden) · [Docs](https://enzyme.garden/docs) · [Discord](https://discord.gg/nhvsqtKjQd) · [Getting Started](#install)

</div>

Enzyme reads a folder of documents — markdown files, Obsidian vaults, team docs, user research, imported collections — and finds the threads that connect them. Not keyword matches. Not similarity. The cross-cutting connections across documents that never reference each other.

It does this by generating **catalysts**: pre-computed questions drawn from the full timeline of each entity in your content. Your agent searches through catalysts instead of grepping through files.

No conversation history needed. No runtime reasoning. The expensive work happens once at init. After that, queries run locally in ~8ms on an on-device embedding model.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
```

On macOS you can also use Homebrew:

```bash
brew install jshph/enzyme/enzyme-cli
```

Then add the Claude Code plugin:

```bash
claude plugin marketplace add jshph/enzyme
claude plugin install enzyme
```

### MCP server

If you prefer MCP over the plugin, Enzyme ships a stdio MCP server that works with any MCP-compatible client (Claude Desktop, Cursor, etc):

```bash
claude mcp add enzyme -- enzyme mcp
```

The MCP server exposes `init`, `petri`, `catalyze`, and `status` tools — you can initialize and explore entirely from the client without running CLI commands separately.

## Quick start

If you have the Claude Code plugin installed, open your content folder in Claude Code and run `/enzyme`. It handles initialization and walks you through your first exploration.

To compile standalone (or without Claude Code):

```bash
cd /path/to/your/content    # any folder of markdown files
enzyme init                  # compiles concept graph — under 15s for 1k docs
```

## What it does

Enzyme reads the structure of your content — tags, links, folders, timestamps — and builds semantic clusters with temporal weight on every entity. From those clusters it generates **catalysts**: thematic questions that cut across your content and surface connections keyword search can't reach.

A search for "why we keep rewriting the auth layer" finds the ADR from six months ago, a retro note about scope creep, and a reading highlight on accidental complexity — even if none of those share keywords with the query.

### Core concepts

- **Entities** — the tags, links, and folders in your content. Each one becomes a semantic cluster.
- **Catalysts** — pre-computed questions Enzyme discovers across your material. Searching through catalysts connects content that keyword and vector search miss.
- **Petri** — the compiled index: what's trending, what entities exist, and what catalysts are anchored to each.
- **Apply** — project your concept graph onto an unfamiliar corpus. `enzyme apply /path/to/other/repo` maps your catalysts onto new content. See [apply docs](https://enzyme.garden/docs/apply/).

### Example: petri output

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

## How Enzyme compares

### vs. keyword and vector search

Keyword search finds mentions. Vector search finds similarity. Neither finds the thread that connects a frustrated aside in a November retro to a confident decision in a March strategy doc — because they share no keywords and aren't semantically similar. They're connected by a tension that evolved over time. Catalysts find that because they're generated from the full timeline of each entity, not from individual documents.

### vs. knowledge graphs (Graphify, etc.)

Knowledge graph tools extract explicit relationships — function calls, imports, entity-relationship triples. Enzyme surfaces implicit connections — themes and tensions that span documents which never reference each other. A knowledge graph tells you DigestAuth depends on Response. A catalyst asks *"What does the commitment to simplicity cost when the pressure to ship keeps winning?"* — and that question reaches both the retro and the ADR because it probes the underlying tension, not the surface structure.

### vs. runtime memory (Mem0, Honcho, etc.)

Runtime memory tools extract facts from conversations as they happen. They start empty and build up over time. Enzyme reads what already exists and extracts the conceptual structure immediately. They solve different problems: runtime memory captures what happens in conversation; Enzyme reads what accumulated before the conversation. If your users import content — collections, saves, transcripts, research — Enzyme gives the agent their conceptual landscape from the first import, with no conversation history required.

### Why compile-time?

Most memory tools build understanding at runtime — they need conversation history before they know anything about your content. Enzyme works the other way: it extracts the conceptual structure from what already exists. The first agent conversation is as rich as the hundredth.

This matters when you're building on imported content (reading highlights, curated collections, user research, team docs). There's no cold start. The intelligence layer is ready from the moment the content is indexed.

## For teams and products

Enzyme is the same engine whether it's running on a personal vault or a product's user corpus. 11MB binary, 23MB embedding model, runs on CPU. No per-query cost, no data leaving your servers. Catalyst generation is the only cloud call: cents per user per refresh, through your own API key.

The compile step gives your users an intelligence layer from day one — no conversation history needed. For deployment details and domain configuration, see [For teams & products](https://enzyme.garden/docs/for-teams/).

## Requirements

- A folder of markdown files — ADRs, docs, Obsidian vaults, team docs, imported collections, any `.md` corpus
- macOS (Apple Silicon or Intel) or Linux (x86_64 or aarch64)
- Works out of the box via [OpenRouter](https://openrouter.ai)'s free tier — or bring your own API key (`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL`)

## Links

- [enzyme.garden](https://enzyme.garden) — website
- [Docs](https://enzyme.garden/docs) — how it works, catalysts, apply, for teams & products
- [Setup guide](https://enzyme.garden/setup) — install and configure
