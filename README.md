# Enzyme

An LSP for your ideas — gives Claude structural understanding of your Obsidian vault so it thinks with you instead of guessing.

## Why not just point Claude at a vault?

An Obsidian vault is a pile of markdown files. Giving an LLM raw file access works, but it searches by keywords and has no sense of what matters to you right now.

Your vault already has a shape. The tags you chose, the links you drew, the folders you sorted things into — those aren't just organization. They're grips: evidence of how you've been thinking. Enzyme reads those signals, builds semantic clusters from them, and generates **catalysts** — thematic questions that reach content those tags alone would never surface.

A search for "feeling stuck on the project" finds notes about creative blocks, momentum, and resistance — even if none of those words appear in the query or the notes.

**Temporal awareness.** Recent ideas are weighted more heavily. Enzyme uses a 30-day half-life decay so that what you've been thinking about *this week* surfaces before notes from two years ago. Your vault has a pulse; Enzyme reads it.

**Vault shape.** Enzyme scans the folders, tags, links, and dates already present in your vault, then lets you confirm the entities and exclusions that should be persisted in config. It bootstraps around the shape of your system rather than needing you to summarize your ideas.

**Runs on your machine.** The embedding model is a 52 MB quantized ONNX file that runs entirely on-device. No data leaves your machine. A background daemon keeps the model warm across CLI invocations and auto-exits after 5 minutes of idle.

## Performance

Benchmarks on an M5 MacBook Pro:

| Vault | Files | Full init |
|-------|-------|-----------|
| 4 years of notes | ~1,000 files | **~15s** |

- **Indexing** — file discovery, entity extraction, and trending score computation happen in-process with no network calls.
- **Embeddings** — computed locally on the ONNX runtime at zero API cost. The model is 23M parameters (BERT variant, int4 quantized) running on 4 threads. The daemon keeps it loaded across invocations so repeat runs skip model load time.
- **Catalysts** — generated on-device when the local model is installed and selected. Without it, Enzyme uses the hosted bootstrap or an explicitly configured provider. Generation runs concurrently across entities.
- **Search** — catalyst-mediated retrieval against precomputed similarities. Sub-second.
- **Disk footprint** — single binary + 52 MB model file. Vault data lives in a SQLite database and a compact binary embeddings file alongside your vault. No Docker, no server.

## Install

```bash
# Default install
curl -fsSL https://raw.githubusercontent.com/useenzyme/enzyme/main/install.sh | bash

# Development install from this repo
make install
```

## Usage

```bash
# Initialize a vault (discovers, indexes, embeds, generates catalysts)
enzyme init -p /path/to/vault

# Install agent-facing instructions after init
enzyme install hermes
enzyme install openclaw
enzyme install codex
enzyme install claude

# Preview and persist first-time config before init
enzyme scan -p /path/to/vault
enzyme scan -p /path/to/vault --write-config

# Quiet mode — no output, just exit code (for agent/programmatic use)
enzyme init -q -p /path/to/vault
enzyme refresh -q -p /path/to/vault

# Voice-ready init — return once seed petri context exists; full init continues in background
enzyme init --voice-ready --voice-entities 3 --voice-min-catalysts 1 -p /path/to/vault

# Search using catalysts
enzyme catalyze "tension between shipping fast and getting it right" -p /path/to/vault

# See trending entities and their catalysts
enzyme petri -p /path/to/vault

# Check vault status
enzyme status -p /path/to/vault
```

## Configuration

**`~/.enzyme/config.toml`** — Entity and catalyst configuration:

```toml
[vaults."/path/to/vault"]
entities = ["#distributed-systems", "[[Raft Consensus]]", "folder:journal"]
frontmatter_link_fields = ["people"]
excluded_tags = ["todo", "template"]
excluded_folders = [".claude", ".enzyme", ".git", ".obsidian", "templates"]
```

`frontmatter_link_fields` promotes plain strings (a scalar or YAML list) from
the named fields into generic link entities. For example, `people: [Alice
Smith, Bob Jones]` becomes the same two entity handles that wikilinks would
produce. Literal `[[wikilinks]]` continue to work in every frontmatter field
without configuration; Enzyme does not infer a typed “person” model from the
field name.

If no entities are configured, Enzyme selects from trending analysis automatically. Legacy vault-local `enzyme-config.yaml` is still read for backward compatibility.

## API keys

Enzyme needs an API key only for catalyst generation. By default, `enzyme init` uses Enzyme's hosted bootstrap and ignores inherited `OPENAI_*` variables so it does not silently spend your personal key. The first configured vault on a machine can initialize without login; refresh, publishing, account credits, and additional vaults require `enzyme login`.

If you intentionally want to bring your own OpenAI-compatible key, pass `--use-env-llm`. In that mode Enzyme reads:

`OPENAI_API_KEY` env var (+ optional `OPENAI_BASE_URL`, `OPENAI_MODEL`)

If no hosted or env key is available, catalyst generation is skipped and the rest of the pipeline (indexing, embedding, local search) still works.

## Using the local model

Builds that ship with the `local-llm` feature can generate catalysts entirely on-device — no API key, no network:

```bash
enzyme model install     # ~88 MB fine-tuned SmolLM2 GGUF into ~/.enzyme/models
enzyme model use catalyst-135m-v3 # require this installed model
enzyme model status      # configured mode, effective provider, model, network use
enzyme init              # catalysts generated in-process
```

Catalyst mode is machine-wide and stored in `~/.enzyme/config.toml`:

```bash
enzyme model auto        # default: selected local model when installed, otherwise hosted
enzyme model use <name>  # require local generation; fail clearly if weights are missing
enzyme model disable     # use hosted generation without removing local weights
enzyme model verify      # verify the selected file and registry checksum
```

`--use-env-llm` remains an explicit one-run override for OpenAI/OpenRouter-compatible credentials; inherited provider variables do not silently change `auto`, `local`, or `hosted` mode. `ENZYME_LOCAL_ENGINE=0` remains a legacy process-level opt-out. Concurrency, threads, and the prompt context budget adapt to the machine; `ENZYME_LLM_CONCURRENCY` and `ENZYME_MAX_CONTEXT_CHARS` still override them.

`enzyme compile` is an explicit OpenRouter Decisions operation. It reuses the
resolved environment provider configuration, including the common
`OPENAI_API_KEY` + `OPENAI_BASE_URL=https://openrouter.ai/api/v1` form. Catalyst
generation uses `OPENAI_MODEL`; Decisions uses `ENZYME_JEV_MODEL` (default
`typesafe/jev-1.13`).

The default fine-tuned weights are hosted on Enzyme's public GitHub release, so
`enzyme model install` requires no account or API token.

## App Plugins

The curl installer is CLI-only. Claude Code and Codex plugins are app-based installs that add first-time setup help on top of the CLI. They do not install automatic refresh or prompt-injection hooks; agents use `enzyme petri`, `enzyme catalyze`, and `enzyme refresh` explicitly when the task calls for them.

## Architecture

~12,800 lines of Rust, ~33% tests.

| Component | Detail |
|-----------|--------|
| Embedding model | MongoDB/mdbr-leaf-ir, 23M params, 384d, ONNX int4 quantized (52 MB) |
| Embedding runtime | ONNX Runtime, 4 threads, Unix socket daemon |
| Search | HNSW + BM25, catalyst-mediated |
| Database | SQLite with WAL mode |
| LLM | Local fine-tuned SmolLM2 via llama.cpp, or hosted Gemini Flash Lite (structured output) |
| Catalyst profiles | dialectical, synthesis, relational, operational, strategic, reflective |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design.

## License

MIT
