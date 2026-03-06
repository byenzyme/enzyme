---
name: enzyme
description: >
  Explore an Obsidian vault using Enzyme — surface connections between ideas,
  find latent patterns across notes. Use when the user wants to explore their
  thinking, draw connections, or search their vault by concept rather than keyword.
allowed-tools: Bash, Read, Glob, Grep
argument-hint: [query or vault-path]
---

# Enzyme — Vault Exploration Skill

## What Enzyme Is

Enzyme turns your Obsidian vault into something you can converse with. It works through three concepts:

**Entities** are the tags (`#travel`), wikilinks (`[[open questions]]`), and folders (`/people`) in your vault. Each one is a semantic cluster — a gathering of content you've already organized by how you think. Hierarchical tags like `#travel/pyrenees` create nested clusters.

**Catalysts** are AI-generated questions anchored to each entity. They probe what's latent in that cluster. A catalyst for `#travel` might be: *"What kept pulling you forward when something was asking you to stay?"* — and content surfaces because it **speaks to the question**, not because it contains matching words. The same entity explored through different catalysts reveals different material.

**Petri** is the live readout of what's growing in your vault — which entities are active, what catalysts have formed around them, where the thinking is heading. Each entity carries temporal metadata: when you last engaged it, how frequently, whether it's active or dormant. Dormant entities are often the most interesting — they surface threads you've stopped noticing.

Content retrieval works by **resonance with catalyst questions**, not keyword matching. The catalysts encode the vault's own vocabulary for its themes — they're handles the vault has grown. Reaching for them connects you to content that generic search terms won't find.

## Prerequisites

The `enzyme` binary must be in PATH. If `enzyme` is not found, tell the user:

> Install enzyme: `curl -fsSL https://raw.githubusercontent.com/jshph/enzyme/main/install.sh | bash`

### Check vault initialization

```bash
ls .enzyme/enzyme.db
```

- If `.enzyme/enzyme.db` exists: vault is ready. Run `enzyme petri` to begin.
- If it doesn't exist: run `enzyme init` to initialize the vault.

## Commands

### `enzyme petri` — See what's growing

Returns JSON with trending entities and their catalysts. Output includes a `stale` field — if `true`, suggest running `enzyme refresh`.

```bash
enzyme petri                  # Default: top 10 entities
enzyme petri -n 5             # Top 5 entities
```

### `enzyme catalyze "query"` — Search by concept

Activates the vault's catalysts to surface resonant content. Returns JSON with matched excerpts, file paths, and contributing catalysts. Output includes a `stale` field.

```bash
enzyme catalyze "feeling stuck"
enzyme catalyze "tension between efficiency and presence" -n 20
```

### `enzyme init` — Initialize a vault

```bash
enzyme init                           # Initialize current directory
enzyme init -p /path/to/vault
enzyme init --guide "vault guide content"
```

### `enzyme refresh` — Update the index

Re-scans vault content. Use when vault content has changed significantly or when commands report `stale: true`.

```bash
enzyme refresh
enzyme refresh --full                 # Force full re-index
```

### `enzyme apply <target>` — Project catalysts onto external content

Indexes an external directory using the current vault's catalysts. After applying,
search the target with `enzyme catalyze "query" --vault <target>`.

```bash
enzyme apply ./research-papers           # Apply current vault's catalysts
enzyme apply ./papers --source ~/vault   # Explicit source vault
```

### `enzyme setup` — Download the embedding model

Downloads the embedding model to `~/.enzyme/models/`. Required before first use.

```bash
enzyme setup                  # Download model if missing
enzyme setup --check          # Exit 0 if present, exit 1 if missing
```

### When to use `catalyze` vs `Grep`

**Use Grep when you have a concrete anchor** — something that exists verbatim in the vault:
- People: "Sarah", `[[Dr. Chen]]`
- Tags: `#productivity`, `#enzyme/pmf`
- Links/titles: `[[On Writing Well]]`, `[[meeting notes]]`
- Files: `Readwise/Articles/...`, book titles, paper names
- Proper nouns: places, companies, projects

**Use `catalyze` when you only have a theme/concept** — no anchor to grep:
- "What have I written about feeling stuck?" (no name, no tag, no title)
- "cost of care in algorithmic interfaces" (academic framing — vault won't use these words)
- "tension between efficiency and presence" (conceptual, not anchored)

The test: would these exact words appear in their notes? Names and tags always do. Abstract/academic language rarely does — vaults use personal, concrete phrasing.

## Workflow

1. **Start with petri.** Run `enzyme petri` to see the landscape — what's active, what's dormant, what catalysts have formed.

2. **Ground in evidence.** Before making observations, use catalysts from the petri to run `enzyme catalyze` searches. Look across entities for patterns — what the user keeps returning to, avoiding, or circling.

3. **Open with a question.** Synthesize a single 10-20 word question that names something the user is *doing* across their vault — then ground it with their words. This is the invitation in. Follow [petri-guide.md](petri-guide.md).

4. **Follow threads.** Use catalysts from petri results to drive searches based on what the user responds to. A catalyst for one entity often surfaces content connecting to another.

5. **Present search results** following [search-guide.md](search-guide.md). Lead with their words from matched excerpts, notice tensions across results, suggest specific next searches using catalyst language.

6. **Check staleness.** If output includes `"stale": true`, suggest running `enzyme refresh` to pick up recent changes.

7. **If the vault isn't initialized** (no `.enzyme/enzyme.db`), tell the user and offer to run `enzyme init`. If the model is missing, run `enzyme setup`.
