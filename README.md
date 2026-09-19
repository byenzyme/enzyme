<div align="center">

# 🧬 Enzyme

[![Discord](https://img.shields.io/discord/1191288276536008745?label=Discord&logo=discord&style=flat-square)](https://discord.gg/nhvsqtKjQd)
[![License](https://img.shields.io/github/license/byenzyme/enzyme?style=flat-square)](LICENSE)
[![Release](https://img.shields.io/github/v/release/byenzyme/enzyme?style=flat-square)](https://github.com/byenzyme/enzyme/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/byenzyme/enzyme/total?style=flat-square&label=Downloads&color=6c757d)](https://github.com/byenzyme/enzyme/releases)

</div>

Enzyme introduces a compile step for your Markdown wiki that:

1. Uses temporally grounded context sampling that captures the contextual use of tags and wikilinks, or natural accumulation in folders (i.e. pseudo-[Zettelkasten](https://zettelkasten.de/introduction/))
2. With this context, generate questions (called **catalysts**) and embed them as semantic routes to the whole knowledge base
3. Keeps refreshing, but separates new doc ingestion (local, fast) from catalyst evolution (cheap, periodic)

When your agent passes queries through the catalysts, it gets a more personalized way to get caught up on the knowledge base.

**New:** Enzyme relies on a program that [**Jev**](https://typesafe.ai/blog/introducing-system-one-models-and-jev) generates through a deterministic scan:

```
profile relationships {
  seek "what matters between people"
  notice ["meaningful exchanges", "shared interests", "unfinished conversations"]
}

vault "~/notes" {
  question budget 40
  sample across time
  favor recent periods

  learn questions from folder "people"
    including linked pages
    about relationships
  learn questions from folder "meetings" about operational
  learn questions from tags ["founding", "ai-ux"] about decisions
  learn questions from folder "inbox"

  project questions into "~/notes/Readwise"

  // Guidance compiled for your agent, not an enforced hook.
  when asked {
    "Use grep for names, titles, and exact phrases."
    retrieve passages through learned questions
    answer with sources
  }
}
```

## Get started

```
curl -fsSL https://raw.githubusercontent.com/byenzyme/enzyme/main/install.sh | bash
cd <your markdown folder root>
# Optional: enzyme login, or configure OpenRouter by setting OPENAI_API_KEY
enzyme compile -v
enzyme install claude # (or: codex, hermes)
enzyme init
```

Enzyme was built for knowledge bases that grow rapidly:

* Agent memory corpora
* Zettelkasten practices in Obsidian that accumulate new dated notes into singular folders
* Meeting transcriptions built around AI-native, Markdown CRM setups.

It's designed to support knowledge captures that might be later be important, even if they don't serve a current task. Enzyme is focused on doing one thing well: **giving agents the tools to make ideas compound**.

And it ships with a set of `profiles` designed around a personal knowledge base, that were refined over [2 years of personal use](https://joshpham.com/kit/how-i-use-enzyme). Here are some examples of how they are used:


| Read...                  | For...                                          | Profile               |
|--------------------------|-------------------------------------------------|-----------------------|
| Project notes            | what's stuck and what keeps blocking            | `operational`         |
| Decision records         | why a choice won, what would change it          | `decision_trace`      |
| Saved articles           | connections to what you're already working on   | `resonance_trace`     |
| Journals                 | what keeps returning across entries             | `reflective`          |
| People notes             | what matters in these relationships             | `relational`          |
| Feedback / activity logs | what works for you, under what constraints      | `preference_evidence` |
| *(default)*              | costs, assumptions, live tensions               | `tension_trace`       |

## Agent tools for retrieval

`enzyme petri` shows what Enzyme found worth asking about. In an interactive terminal it renders a tree; piped, it emits JSON:

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

Each entity carries catalysts spanning different eras — questions that cut across months of content.

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

*Output above is illustrative — it shows the shape of a result, not a captured run.*

## message archives (_experimental_)

Enzyme indexes SQLite tables alongside Markdown — iMessage, WhatsApp, Mail, or any table of dated rows.

None of these is a special case. A `handle_id` column is repeated person values across dated rows, the same way `[[links]]` are repeated person values across dated notes. Both collapse to entity occurrences with effective dates, and nothing downstream knows which one it came from: the same profiles, budgets, and catalysts apply to a message thread and a folder of meeting notes.

A source names columns by the role they play rather than by the app they came from:

| Role     | Means                                           |
|----------|-------------------------------------------------|
| `id`     | the row's identity                              |
| `who`    | participants; scalar, JSON array, or delimited  |
| `when`   | the row's timestamp                             |
| `what`   | the text to read                                |
| `where`  | the container the row belongs to (optional)     |
| `weight` | numeric significance per occurrence (optional)  |

## API keys

Enzyme needs an API key only for catalyst generation and for `enzyme compile`'s selection step. By default `enzyme init` uses Enzyme's hosted bootstrap and ignores inherited `OPENAI_*` variables so it does not spend your personal key. Credential resolution is explicit key → configured local model → anonymous brokered free config, with no additional configuration.

The first configured vault on a machine initializes without login. Refresh, publishing, account credits, and additional vaults require `enzyme login`.

To bring your own OpenAI-compatible key, pass `--use-env-llm`, which reads `OPENAI_API_KEY` plus optional `OPENAI_BASE_URL` and `OPENAI_MODEL`. Without any hosted or env key, catalyst generation is skipped and indexing, embedding, and local search still work.

`enzyme compile` is an explicit OpenRouter Decisions operation. It reuses the hosted lease from `enzyme login` and the free-config broker; an explicit `OPENAI_API_KEY` with `OPENAI_BASE_URL=https://openrouter.ai/api/v1` takes precedence. Catalyst generation uses `OPENAI_MODEL`; Decisions uses `ENZYME_JEV_MODEL` (default `typesafe/jev-1.13`).
