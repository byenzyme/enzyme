# Enzyme

Enzyme controls how an agent reads your notes.

It reads the structure already in a vault — tags, links, folders, dates — and compiles it into **catalysts**: pre-computed questions an agent searches through instead of grepping files. Which questions it generates depends on how you tell it to read, and you say that in a `.enzyme` program:

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
  learn questions from tags ["startup", "design"] about decisions
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

For most vaults this is the whole configuration format, replacing `config.toml`; `enzyme init` reads it directly. Named workspaces and structured sources still use `config.toml` (see [Message archives](#message-archives) and [Migrating](#migrating-from-configtoml)).

Enzyme is an intelligence layer, not a harness. It has no tools, permissions, approvals, or API surface for taking actions. It decides what your agent reads, not what it does.

## Reading profiles

A profile tells Enzyme what to look for. The same notes read for what's stuck produce different questions than the same notes read for why a decision won.

| Read...                  | For...                                          | Profile               |
|--------------------------|-------------------------------------------------|-----------------------|
| Project notes            | what's stuck and what keeps blocking            | `operational`         |
| Decision records         | why a choice won, what would change it          | `decision_trace`      |
| Saved articles           | connections to what you're already working on   | `resonance_trace`     |
| Journals                 | what keeps returning across entries             | `reflective`          |
| People notes             | what matters in these relationships             | `relational`          |
| Feedback / activity logs | what works for you, under what constraints      | `preference_evidence` |
| *(default)*              | costs, assumptions, live tensions               | `tension_trace`       |

Each has a shorter alias in the language: `relationships`, `decisions`, `resonance`, `preferences`, `reflection`, `tensions`. `operational` is the same either way.

Omit `about` and Enzyme picks a profile from the entity's name, folder, and content.

Run `enzyme spec profiles` to print the full built-in definitions.

## What a reading produces

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

Each entity carries catalysts spanning different eras — questions that cut across months of writing. These are what the agent searches, not your raw text. Every catalyst also carries a `receipts` block naming the documents behind it.

Searching goes through those questions rather than around them:

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

The query shares no keywords with the retro or the ADR. The catalyst bridged them: the retro said "scope creep," the ADR said "separation of concerns," and the underlying tension was the same.

*The examples above are illustrative, in the same spirit as the program at the top — they show the shape of the output, not a captured run against a specific vault.*

## Install

```bash
# Default install
curl -fsSL https://raw.githubusercontent.com/useenzyme/enzyme/main/install.sh | bash

# Development install from this repo
make install
```

## Getting started

```bash
cd /path/to/your/vault
enzyme compile -v      # propose a setup and print it
$EDITOR ~/.enzyme/configs/*.enzyme
enzyme init            # index, embed, generate catalysts
```

`enzyme compile` scans the vault, chooses which folders, tags, and pages are worth reading, assigns each a profile, and writes a `.enzyme` program to `~/.enzyme/configs/<vault>-<hash>.enzyme`. It does not index anything or generate questions — that's `enzyme init`. Use `--dry-run` to print a proposal without saving, or `--output` to write it elsewhere.

Selection is a model call (OpenRouter Decisions, see [API keys](#api-keys)); rendering the program is deterministic. If the vault already has a readable setup, compile validates it and leaves the file untouched, comments included.

Edit the generated program before running `init`. Later edits are picked up by `init` and `refresh` directly; rerunning `compile` is optional revalidation, not regeneration.

### Migrating from config.toml

```bash
enzyme spec migrate
```

Writes `settings.enzyme` plus one file per vault and renames `config.toml` to a timestamped backup. It validates every setting before publishing and refuses rather than dropping anything it can't express — named workspaces and SQLite sources still need `config.toml`.

Legacy `config.toml` keeps working. Readable files take precedence where both describe the same vault. An invalid `.enzyme` file is an error, not a fallback to the old config.

## Usage

```bash
# Search using catalysts
enzyme catalyze "tension between shipping fast and getting it right"

# See trending entities and their catalysts
enzyme petri

# Incremental refresh after the vault changes
enzyme refresh

# Check vault status
enzyme status

# Install agent-facing instructions after init
enzyme install claude    # or: codex, hermes, openclaw
```

Every command takes `-p /path/to/vault` (defaults to the current directory) and `-q` for quiet, exit-code-only operation.

```bash
# Return once seed context exists; full init continues in the background
enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1
```

### Inspecting a program

```bash
enzyme spec compile config.enzyme                # resolved plan as JSON
enzyme spec compile config.enzyme --instructions # the agent policy it compiles to
enzyme spec inspect "$PWD"                       # effective config for a vault
enzyme spec serve                                # syntax playground on :4178
```

The playground runs the same Rust parser as the runtime.

## Worth knowing

The full syntax is documented in [tools/reading-spec/README.md](tools/reading-spec/README.md). These are the parts that change what you can do.

**Custom profiles.** When none of the seven fit, define one inline or as a reusable block:

```
learn questions from folder "inbox" about profile {
  seek "what is emerging across scattered thoughts"
  notice ["recurring ideas", "unexpected connections", "open questions"]
}
```

A profile defined in any `.enzyme` file under `~/.enzyme/configs/` is usable from every other file.

**`including linked pages`** keeps the folder and also selects eligible pages under it, below the usual density threshold. Children inherit the parent's profile. This is how you get per-person or per-project readings without listing each one.

**Budgets are per-entity, not totals.** `question budget N` sets how many catalysts the *largest* entity receives; everything else scales down by its share of documents, with a floor of three. Global `learning` sets defaults, a vault overrides them, and a per-reading block overrides that:

```
learning {
  sample across time
  favor recent periods
  question budget 40
}

vault "~/notes" {
  learn questions from folder "archive" about decisions {
    favor all periods equally
    question budget 20
  }
}
```

**`project questions into "path"`** registers an external target, so you can search an unfamiliar repo or corpus through your own vault's questions.

**`log "name"`** marks a single file of dated entries as its own entity, so retrieval sees its internal timeline instead of treating the file as one document.

**`when asked { … }`** compiles ordered retrieval and citation guidance into agent instructions. `remember in "observations.md" when { … }` does the same for capture conditions. Both compile to instructions, not hooks — Enzyme writes guidance an agent may follow, and does not execute them itself.

## What it actually reads

Enzyme samples rather than reading everything. Hosted generation aims for about one excerpt per requested catalyst, keeps at least 10 available for small entities, and caps at 40.

Each catalyst carries a `receipts` block naming the documents behind it; the text is canonical and the tables are a rebuildable index. Where evidence is thin — too few distinct documents or too little substantive text — Enzyme emits nothing rather than a generic question. An entity producing no catalysts is usually correct behavior, not a failure.

Broad recurring themes retrieve well immediately. Narrow queries anchored to one person or one year depend on entity anchoring and are more sensitive to how the vault names things.

## Message archives

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

An omitted or NULL `weight` counts as one, the same as a single Markdown mention.

Structured sources are configured in `config.toml` today; `enzyme spec migrate` refuses to convert them rather than dropping detail it cannot yet express.

## Why compile-time?

Most memory tools build understanding at runtime and need conversation history first. Enzyme extracts structure from what already exists, so there is no cold start. This matters most for imported material — reading highlights, curated collections, research corpora — where no interaction history exists.

**Temporal awareness.** Recent material is weighted more heavily via a 30-day half-life decay.

**Runs on your machine.** The embedding model is a 52 MB quantized ONNX file that runs on-device; no data leaves your machine for indexing or search. A background daemon keeps it warm across CLI invocations and exits after 5 minutes idle. The default build links no keychain, keyring, or Security Framework.

## Performance

Benchmarks on an M5 MacBook Pro:

| Vault | Files | Full init |
|-------|-------|-----------|
| 4 years of notes | ~1,000 files | **~15s** |

- **Indexing** — file discovery, entity extraction, and trending scores, in-process with no network calls.
- **Embeddings** — computed locally on the ONNX runtime at zero API cost. 23M parameters (BERT variant, int4 quantized) on 4 threads.
- **Catalysts** — generated on-device when a local model is installed and selected, otherwise hosted. Runs concurrently across entities.
- **Search** — catalyst-mediated retrieval against precomputed similarities. Sub-second.
- **Disk footprint** — single binary plus a 52 MB model file. Vault data lives in SQLite and a compact binary embeddings file. No Docker, no server.

## API keys

Enzyme needs an API key only for catalyst generation and for `enzyme compile`'s selection step. By default `enzyme init` uses Enzyme's hosted bootstrap and ignores inherited `OPENAI_*` variables so it does not spend your personal key. Credential resolution is explicit key → configured local model → anonymous brokered free config, with no additional configuration.

The first configured vault on a machine initializes without login. Refresh, publishing, account credits, and additional vaults require `enzyme login`.

To bring your own OpenAI-compatible key, pass `--use-env-llm`, which reads `OPENAI_API_KEY` plus optional `OPENAI_BASE_URL` and `OPENAI_MODEL`. Without any hosted or env key, catalyst generation is skipped and indexing, embedding, and local search still work.

`enzyme compile` is an explicit OpenRouter Decisions operation. It reuses the hosted lease from `enzyme login` and the free-config broker; an explicit `OPENAI_API_KEY` with `OPENAI_BASE_URL=https://openrouter.ai/api/v1` takes precedence. Catalyst generation uses `OPENAI_MODEL`; Decisions uses `ENZYME_JEV_MODEL` (default `typesafe/jev-1.13`).

## Using the local model

Builds shipping the `local-llm` feature generate catalysts entirely on-device, with no API key and no network:

```bash
enzyme model install              # ~88 MB fine-tuned SmolLM2 GGUF into ~/.enzyme/models
enzyme model use catalyst-135m-v3 # require this installed model
enzyme model status               # configured mode, effective provider, network use
enzyme init                       # catalysts generated in-process
```

Catalyst mode is machine-wide:

```bash
enzyme model auto        # default: local when installed, otherwise hosted
enzyme model use <name>  # require local; fail if weights are missing
enzyme model disable     # hosted generation without removing local weights
enzyme model verify      # verify the selected file and registry checksum
```

`--use-env-llm` is a one-run override; inherited provider variables do not change `auto`, `local`, or `hosted` mode. Concurrency, threads, and the prompt context budget adapt to the machine, with `ENZYME_LLM_CONCURRENCY` and `ENZYME_MAX_CONTEXT_CHARS` as overrides. Default weights are hosted on Enzyme's public GitHub release, so `enzyme model install` needs no account or token.

## App plugins

The curl installer is CLI-only. Claude Code and Codex plugins are app-based installs that add first-time setup help on top of the CLI. They install no automatic refresh and no prompt-injection hooks; agents call `enzyme petri`, `enzyme catalyze`, and `enzyme refresh` explicitly.

## Architecture

~74,000 lines of Rust, ~38% tests.

| Component | Detail |
|-----------|--------|
| Configuration | `.enzyme` language, single Rust parser shared by runtime, CLI, and playground |
| Embedding model | MongoDB/mdbr-leaf-ir, 23M params, 384d, ONNX int4 quantized (52 MB) |
| Embedding runtime | ONNX Runtime, 4 threads, Unix socket daemon |
| Search | HNSW + BM25, catalyst-mediated |
| Database | SQLite with WAL mode |
| Catalyst generation | Local fine-tuned SmolLM2 via llama.cpp, or hosted Gemini Flash via OpenRouter (structured output) |
| Setup selection (`enzyme compile`) | OpenRouter Decisions, `typesafe/jev-1.13` |
| Catalyst profiles | `tension_trace`, `resonance_trace`, `decision_trace`, `relational`, `operational`, `reflective`, `preference_evidence` |
| Catalyst formats | `question`, `thesis`, `claim` |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and
[tools/reading-spec/README.md](tools/reading-spec/README.md) for the language's
runtime and migration details.

## License

MIT
