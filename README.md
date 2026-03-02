# enzyme-rust

Rust rewrite of [Enzyme](https://github.com/jshph/enzyme) — catalyst generation and semantic search for Obsidian vaults.

## Setup

Requires the [mdbr-leaf-ir](https://huggingface.co/MongoDB/mdbr-leaf-ir) embedding model (int4 quantized ONNX, 52 MB). The binary is stored locally in `models/` but gitignored — `tokenizer.json` is tracked.

```bash
# Install model to ~/.enzyme/models/ and build the CLI
make install

# Or just the model
make install-model

# Verify model is in place
make check-model
```

If `models/model.onnx` is missing (fresh clone), see [`models/README.md`](models/README.md) for re-download instructions.

## Codebase

~8,800 lines of Rust, ~33% tests.

```
enzyme-core   8,254 loc
  document      1,985    frontmatter, chunking, extraction, discovery
  db            1,588    sqlite repository, schema, migrations
  embedding     1,268    ort/rten backends, normalization
  search        1,054    bm25, hnsw
  pipeline      1,028    catalyst generation, indexing, similarity
  config          797    vault + global config, paths
  llm             278    api client, prompt templates
  models          246    entity, document, catalyst, embedding
enzyme-cli      505 loc  clap commands (init, catalyze, status, etc.)
enzyme-api        1 loc  stub
```
