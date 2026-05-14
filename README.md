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
- **Catalysts** — the only network call. Uses a small, fast model (Gemini Flash Lite via OpenRouter). Concurrent generation across all entities. A full vault refresh costs a few cents.
- **Search** — catalyst-mediated retrieval against precomputed similarities. Sub-second.
- **Disk footprint** — single binary + 52 MB model file. Vault data lives in a SQLite database and a compact binary embeddings file alongside your vault. No Docker, no server.

## Install

```bash
# Default install
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash

# Development install from this repo
make install
```

## Usage

```bash
# Initialize a vault (discovers, indexes, embeds, generates catalysts)
enzyme init -p /path/to/vault

# Install agent-facing instructions after init
enzyme agents codex
enzyme agents claude

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
excluded_tags = ["todo", "template"]
excluded_folders = [".claude", ".enzyme", ".git", ".obsidian", "templates"]
```

If no entities are configured, Enzyme selects from trending analysis automatically. Legacy vault-local `enzyme-config.yaml` is still read for backward compatibility.

## API keys

Enzyme needs an API key only for catalyst generation. Resolution order:

1. `OPENAI_API_KEY` env var (+ optional `OPENAI_BASE_URL` for custom endpoints)
2. `OPENROUTER_API_KEY` env var
3. `~/.enzyme/auth.json` → `{"openrouter_api_key": "..."}`

If no key is found, catalyst generation is skipped and the rest of the pipeline (indexing, embedding, local search) still works.

## App Plugins

The curl installer is CLI-only. Claude Code and Codex plugins are app-based installs that add first-time setup help and lifecycle refresh hooks on top of the CLI.

## Architecture

~12,800 lines of Rust, ~33% tests.

| Component | Detail |
|-----------|--------|
| Embedding model | MongoDB/mdbr-leaf-ir, 23M params, 384d, ONNX int4 quantized (52 MB) |
| Embedding runtime | ONNX Runtime, 4 threads, Unix socket daemon |
| Search | HNSW + BM25, catalyst-mediated |
| Database | SQLite with WAL mode |
| LLM | Gemini Flash Lite via OpenRouter (structured output) |
| Catalyst profiles | dialectical, synthesis, relational, operational, strategic, reflective |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design.

## License

MIT
