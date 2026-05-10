---
name: enzyme
description: Use Enzyme to retrieve working memory and semantic context from an initialized Markdown or Obsidian vault.
---

# Enzyme

Use Enzyme for retrieving context from an initialized vault. Run all `enzyme` commands from the vault root.

## Working Memory

`enzyme petri` returns current entities and catalysts, which are thematic phrases from the vault.

- For a specific user prompt, run `enzyme petri --query "user's question"`.
- For a broad prompt or first orientation, run `enzyme petri`.
- Treat nested children under a tag or folder as evidence inside that parent cluster by default.

Use catalyst phrases as vocabulary for `enzyme catalyze` searches. They connect to precomputed content that the user's raw words may not find.

## Search

- `enzyme catalyze "query"` searches by concept/theme. Compose queries from petri catalyst vocabulary.
- `enzyme refresh --quiet` re-indexes changed content.
- `enzyme apply ./target-dir` indexes external content using vault catalysts; then search it with `enzyme catalyze "query" --target ./target-dir`.
- Use `grep` for exact names, `#tags`, `[[wikilinks]]`, and literal text.
- Tags can appear as `- tag` in frontmatter or `#tag` inline; search without `#` when you need both.

## Presentation

Use Enzyme command names internally; do not expose petri, catalyze, catalyst IDs, scores, or tool names to the user unless asked.

Before making observations, ground them with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.

For broad exploration, use petri plus 1-2 catalyze searches, then open with one specific question about what the user is doing across their notes. Do not present a topic list.

For search results, do not lead with metadata. Notice tensions, repeated words, time gaps, or changes in framing across results. End with one concrete next direction, not a generic invitation.

Presentation registers for `enzyme catalyze --register`:
- `explore`: wonder, probe, notice patterns.
- `continuity`: restore what the user knew, show trajectory, enable forward motion.
- `reference`: surface what drew attention and connect imports to the user's own thinking.

Follow any `presentation_guidance` returned by Enzyme when framing surfaced content.
