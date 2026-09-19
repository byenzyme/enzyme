# Enzyme

Your notes are already the memory. Enzyme is how you program the way an agent reads them.

A vault is a pile of markdown files. Pointing an LLM at it works, but it searches by keyword and has no sense of what matters to you. Enzyme reads the shape you already made — tags, links, folders, dates — and compiles it into **catalysts**: pre-computed questions an agent searches through instead of grepping your files.

What questions it asks depends on how you tell it to read. You say that in a `.enzyme` program:

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

  when asked {
    "Use grep for names, titles, and exact phrases."
    retrieve passages through learned questions
    answer with sources
  }
}
```

That is the whole configuration format. It replaces `config.toml`, and `enzyme init` reads it directly.

The point of writing it down is that reading is a choice, and choices should be legible. Project notes read for what's stuck produce different questions than the same notes read for why a decision won. Before, that choice lived in a TOML key. Now it is a line you can read, edit, and disagree with.

> **Honest about what it isn't.** Enzyme is an intelligence layer, not a harness. It has no action plane — no tools, no permissions, no approvals, no API surface for doing things. It decides what your agent reads, not what it does.

## Reading profiles

A profile tells Enzyme what to look for. A reading is that profile applied to your material. Catalysts are the concrete questions it found worth asking.

| Read...                  | For...                                          | Profile               |
|--------------------------|-------------------------------------------------|-----------------------|
| Project notes            | what's stuck and what keeps blocking            | `operational`         |
| Decision records         | why a choice won, what would change it          | `decision_trace`      |
| Saved articles           | connections to what you're already working through | `resonance_trace`  |
| Journals                 | what keeps returning across entries             | `reflective`          |
| People notes             | what matters in these relationships             | `relational`          |
| Feedback / activity logs | what works for you, under what constraints      | `preference_evidence` |
| *(default)*              | costs, assumptions, live tensions               | `tension_trace`       |

Each has a friendlier alias in the language — `relationships`, `decisions`, `resonance`, `preferences`, `reflection`, `tensions` — and `operational` is spelled the same either way. Omit `about` entirely and Enzyme infers the profile from the entity's name, folder, and content.

Define your own inline when none of them fit:

```
learn questions from folder "inbox" about profile {
  seek "what is emerging across scattered thoughts"
  notice ["recurring ideas", "unexpected connections", "open questions"]
}
```

Run `enzyme spec profiles` to print the full built-in definitions, including the guidance and examples each one carries.

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

`enzyme compile` scans the vault, chooses which folders, tags, and pages are worth reading, assigns each a profile, and writes a `.enzyme` program to `~/.enzyme/configs/<vault>-<hash>.enzyme`. It does not index anything or generate questions — that's `enzyme init`. Use `--dry-run` to print a proposal without saving, or `--output` to write it somewhere else.

Selection is a model call (OpenRouter Decisions, see [API keys](#api-keys)); rendering the program is deterministic. If you already have a readable setup for the vault, compile validates it and leaves your file untouched, comments and all.

The generated program is a starting point, not a verdict. Read it, change the profiles that are wrong, delete the readings you don't want, and run `enzyme init`. Later edits are picked up by `init` and `refresh` directly — rerunning `compile` is optional revalidation, not regeneration.

### Migrating from config.toml

```bash
enzyme spec migrate
```

This writes `settings.enzyme` plus one file per vault and renames `config.toml` to a timestamped backup. It validates every setting before publishing and refuses rather than silently dropping anything it can't express — named workspaces and SQLite sources still need `config.toml` today. Pass `--embed-profiles-in <vault>` to inline the full built-in profile definitions into that vault's file instead of `profiles.enzyme`.

Legacy `config.toml` keeps working. Readable files take precedence where both describe the same vault, and an invalid `.enzyme` file is a hard error — Enzyme will not quietly fall back to the old config.

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

The playground is backed by the same Rust parser the runtime uses; there is no second implementation to drift.

## Language reference

**Sources.** `folder`, `tag`, `link`, and `log`; plurals (`folders`, `tags`) accept lists. `log` marks a single file of dated entries as its own entity so retrieval sees its internal timeline. `including linked pages` keeps the folder and also selects eligible pages under it, which inherit the parent's profile unless you say otherwise.

**Budgets.** `question budget N` sets how many catalysts the *largest* entity receives; everything else scales down by its share of documents, with a floor of three. It is not a total across all readings. Global `learning { … }` sets defaults, a vault block overrides them, and a per-reading block overrides that:

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

**Other declarations.**

- `leave out folders [...]`, `leave out tags [...]`, `leave out links [...]` — excluded material is never indexed, not merely hidden. Tooling directories (`.git`, `node_modules`, `target`, `__pycache__`, and friends) are always excluded and don't need naming.
- `prepare up to N documents per run newest first` — embedding cap; `0` means unlimited.
- `references in fields ["people"]` — promotes plain frontmatter strings to link entities. Literal `[[wikilinks]]` work in any field without configuration.
- `produce questions` / `produce theses` / `produce claims` — output format.
- `project questions into "path"` — register an external retrieval target.
- `remember in "observations.md" when { … }` — compiles capture conditions into agent guidance. Enzyme does not execute these itself; it writes instructions an agent may follow.
- `when asked { … }` — ordered retrieval and citation guidance, compiled to instructions rather than hooks.

Machine-level settings live separately, usually in `settings.enzyme`:

```
settings {
  generation hosted
  updates enabled
  embedding limit 1024
  selection limit 30
}
```

Every `.enzyme` file directly inside `~/.enzyme/configs/` shares one namespace, so a profile defined in `profiles.enzyme` is usable from any vault file. Duplicate names, duplicate vaults, and duplicate global settings are errors. File order never affects precedence.

## What it actually reads

Enzyme samples rather than reading everything. Hosted generation aims for roughly one excerpt per requested catalyst, keeps at least 10 available for small entities, and caps at 40 — so a large vault contributes hundreds of sampled excerpts per run, not its entire contents.

Catalysts are self-auditing. The text is canonical and the tables are a rebuildable index; each catalyst carries a `receipts` block naming the documents behind it. Where the evidence is thin — too few distinct documents or too little substantive text — Enzyme emits **nothing** rather than a plausible-sounding question. An entity producing no catalysts is usually correct behavior, not a failure.

Broad recurring themes retrieve well immediately. Narrow queries anchored to one person or one year depend on entity anchoring and are more sensitive to how your vault names things.

## Why compile-time?

Most memory tools build understanding at runtime and need conversation history before they know anything about your content. Enzyme extracts the structure from what already exists, so the first conversation is as rich as the hundredth. That matters most for imported material — reading highlights, curated collections, research corpora — where there is no interaction history to learn from.

**Temporal awareness.** Recent material is weighted more heavily via a 30-day half-life decay, so what you've been thinking about this week surfaces before notes from two years ago.

**Runs on your machine.** The embedding model is a 52 MB quantized ONNX file that runs entirely on-device; no data leaves your machine for indexing or search. A background daemon keeps it warm across CLI invocations and exits after 5 minutes idle. The default build links no keychain, keyring, or Security Framework at all.

## Performance

Benchmarks on an M5 MacBook Pro:

| Vault | Files | Full init |
|-------|-------|-----------|
| 4 years of notes | ~1,000 files | **~15s** |

- **Indexing** — file discovery, entity extraction, and trending scores, in-process with no network calls.
- **Embeddings** — computed locally on the ONNX runtime at zero API cost. 23M parameters (BERT variant, int4 quantized) on 4 threads.
- **Catalysts** — generated on-device when a local model is installed and selected; otherwise hosted. Runs concurrently across entities.
- **Search** — catalyst-mediated retrieval against precomputed similarities. Sub-second.
- **Disk footprint** — single binary plus a 52 MB model file. Vault data lives in SQLite and a compact binary embeddings file. No Docker, no server.

## API keys

Enzyme needs an API key only for catalyst generation and for `enzyme compile`'s selection step. By default `enzyme init` uses Enzyme's hosted bootstrap and ignores inherited `OPENAI_*` variables so it never silently spends your personal key. Credential resolution is explicit key → configured local model → anonymous brokered free config, with no new configuration knobs.

The first configured vault on a machine initializes without login; refresh, publishing, account credits, and additional vaults require `enzyme login`.

To bring your own OpenAI-compatible key, pass `--use-env-llm`, which reads `OPENAI_API_KEY` (plus optional `OPENAI_BASE_URL`, `OPENAI_MODEL`). Without any hosted or env key, catalyst generation is skipped and indexing, embedding, and local search still work.

`enzyme compile` is an explicit OpenRouter Decisions operation. It reuses the Enzyme-managed hosted lease from `enzyme login` and the free-config broker; an explicit `OPENAI_API_KEY` with `OPENAI_BASE_URL=https://openrouter.ai/api/v1` takes precedence. Catalyst generation uses `OPENAI_MODEL`; Decisions uses `ENZYME_JEV_MODEL` (default `typesafe/jev-1.13`).

## Using the local model

Builds shipping the `local-llm` feature generate catalysts entirely on-device — no API key, no network:

```bash
enzyme model install              # ~88 MB fine-tuned SmolLM2 GGUF into ~/.enzyme/models
enzyme model use catalyst-135m-v3 # require this installed model
enzyme model status               # configured mode, effective provider, network use
enzyme init                       # catalysts generated in-process
```

Catalyst mode is machine-wide:

```bash
enzyme model auto        # default: local when installed, otherwise hosted
enzyme model use <name>  # require local; fail clearly if weights are missing
enzyme model disable     # hosted generation without removing local weights
enzyme model verify      # verify the selected file and registry checksum
```

`--use-env-llm` is an explicit one-run override; inherited provider variables never silently change `auto`, `local`, or `hosted` mode. Concurrency, threads, and the prompt context budget adapt to the machine, with `ENZYME_LLM_CONCURRENCY` and `ENZYME_MAX_CONTEXT_CHARS` as overrides. Default weights are hosted on Enzyme's public GitHub release, so `enzyme model install` needs no account or token.

## App plugins

The curl installer is CLI-only. Claude Code and Codex plugins are app-based installs that add first-time setup help on top of the CLI. They install no automatic refresh and no prompt-injection hooks; agents call `enzyme petri`, `enzyme catalyze`, and `enzyme refresh` explicitly when a task calls for them.

## Architecture

~12,800 lines of Rust, ~33% tests.

| Component | Detail |
|-----------|--------|
| Configuration | `.enzyme` language, single Rust parser shared by runtime, CLI, and playground |
| Embedding model | MongoDB/mdbr-leaf-ir, 23M params, 384d, ONNX int4 quantized (52 MB) |
| Embedding runtime | ONNX Runtime, 4 threads, Unix socket daemon |
| Search | HNSW + BM25, catalyst-mediated |
| Database | SQLite with WAL mode |
| LLM | Local fine-tuned SmolLM2 via llama.cpp, or hosted Gemini Flash Lite (structured output) |
| Catalyst profiles | `tension_trace`, `resonance_trace`, `decision_trace`, `relational`, `operational`, `reflective`, `preference_evidence` |
| Catalyst formats | `question`, `thesis`, `claim` |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and
[tools/reading-spec/README.md](tools/reading-spec/README.md) for the language's
runtime and migration details.

## License

MIT
