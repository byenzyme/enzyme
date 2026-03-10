# Enzyme

An LSP for your ideas — gives Claude structural understanding of your Obsidian vault so it thinks with you instead of guessing.

## Why not just point Claude at a vault?

An Obsidian vault is a pile of markdown files. Giving an LLM raw file access works, but it searches by keywords and has no sense of what matters to you right now.

Your vault already has a shape. The tags you chose, the links you drew, the folders you sorted things into — those aren't just organization. They're grips: evidence of how you've been thinking. Enzyme reads those signals, builds semantic clusters from them, and generates **catalysts** — thematic questions that reach content those tags alone would never surface.

A search for "feeling stuck on the project" finds notes about creative blocks, momentum, and resistance — even if none of those words appear in the query or the notes.

**Temporal awareness.** Recent ideas are weighted more heavily. Enzyme uses a 30-day half-life decay so that what you've been thinking about *this week* surfaces before notes from two years ago. Your vault has a pulse; Enzyme reads it.

**Vault guide.** You can pass Enzyme a description of how your vault is *structured* — what the folders mean, how tags are used, what the organizational logic is. Enzyme takes that structure and derives what the vault is about on its own. It bootstraps around the shape of your system rather than needing you to summarize your ideas.

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
# Build and install (requires Rust toolchain)
make install

# This copies the embedding model to ~/.enzyme/models/ and builds the CLI
```

## Usage

```bash
# Initialize a vault (discovers, indexes, embeds, generates catalysts)
enzyme init -p /path/to/vault

# Pass a vault guide describing how the vault is structured
enzyme init -p /path/to/vault --guide "Folders: journal/ is daily logs,
projects/ has one subfolder per active project, people/ tracks relationships.
Tags: #open-question marks unresolved threads, #synthesis is for connecting ideas."

# Or pipe a guide from a file and initialize within the directory
cat vault-guide.txt | enzyme init

# Quiet mode — no output, just exit code (for agent/programmatic use)
enzyme init -q -p /path/to/vault
enzyme refresh -q -p /path/to/vault

# Search using catalysts
enzyme catalyze "tension between shipping fast and getting it right" -p /path/to/vault

# See trending entities and their catalysts
enzyme petri -p /path/to/vault

# Check vault status
enzyme status -p /path/to/vault
```

## Configuration

**`<vault>/enzyme-config.yaml`** — Entity and catalyst configuration:

```yaml
catalystProfile: dialectical    # or synthesis, relational, operational, strategic, reflective

catalyzedEntities:
  - entity: "#distributed-systems"
  - entity: "[[Raft Consensus]]"
  - entity: "folder:journal"
    catalystProfile: reflective  # per-entity profile override
```

If no entities are configured, Enzyme selects from trending analysis automatically.

## API keys

Enzyme needs an API key only for catalyst generation. Resolution order:

1. `OPENAI_API_KEY` env var (+ optional `OPENAI_BASE_URL` for custom endpoints)
2. `OPENROUTER_API_KEY` env var
3. `~/.enzyme/auth.json` → `{"openrouter_api_key": "..."}`

If no key is found, catalyst generation is skipped and the rest of the pipeline (indexing, embedding, local search) still works.

## Claude Code plugin

Enzyme ships as a [Claude Code plugin](https://github.com/jshph/enzyme) that wraps the CLI. The plugin installs the binary automatically and provides a `/enzyme` skill for vault exploration, semantic search, and artifact creation.

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
