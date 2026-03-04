# Enzyme

Local-first knowledge indexing — explore your Obsidian vault by concept, not keyword.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash
enzyme setup
```

On macOS you can also use Homebrew:

```bash
brew install jshph/enzyme/enzyme-cli
enzyme setup
```

Then add the Claude Code plugin:

```bash
claude plugin marketplace add jshph/enzyme
claude plugin install enzyme
```

## Quick start

```bash
cd /path/to/your/vault
enzyme init
```

Inside Claude Code, invoke `/enzyme` to explore your vault.

## What it does

Your vault already has a shape. The tags you chose, the links you drew, the folders you sorted things into — those aren't just organization. They're evidence of how you've been thinking. Enzyme reads those signals, builds semantic clusters, and generates **catalysts** — thematic questions that surface content those tags alone would never find.

A search for "feeling stuck on the project" finds notes about creative blocks, momentum, and resistance — even if none of those words appear in the query or the notes.

### Core concepts

- **Entities** — the tags, links, and folders you already use. Each one becomes a semantic cluster organized by how you think.
- **Catalysts** — compressed themes Enzyme discovers across your writing. They're the handles your vault has grown; searching through them connects you to content keyword matching won't find.
- **Petri** — a live view of what's trending and the catalysts anchored to each entity.

## Requirements

- An Obsidian vault
- macOS (Apple Silicon or Intel) or Linux (x86_64 or aarch64)
- An OpenAI or OpenRouter API key for catalyst generation (`OPENAI_API_KEY` or `OPENROUTER_API_KEY`)

## Links

- [enzyme.garden](https://enzyme.garden)
- [enzyme-rust](https://github.com/jshph/enzyme-rust) — source code
