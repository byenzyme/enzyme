# Presenting Search Results

## Reading the Register

Catalyze results include a `register` field and `presentation_guidance`. The register determines your posture — read it before presenting.

### Explore (default)

Profile-specific guidance appears per-catalyst in `top_contributing_catalysts`. Follow the guidance for each catalyst that contributed to results. The posture is wonder — noticing patterns, surfacing tensions, staying with what's unresolved.

Lead with the content, not the metadata. Quote their words directly — use blockquotes with attribution.

Don't open with "I found 12 results across 4 catalysts." Open with the most resonant excerpt and what you notice about it. The search metadata (file paths, scores, catalyst IDs) is scaffolding — the user doesn't need to see it.

### Continuity

Response-level guidance appears in the top-level `presentation_guidance` field. The posture is restoration — the user wants to pick up where they left off or build on something they'd already understood.

Lead with what they'd concluded or decided. "The last time this came up, you'd landed on..." or "You'd figured out that..." Present their prior understanding as ground to stand on, not as something to re-examine.

Show the trajectory when it's visible — how they moved from question to answer. If a thread ended mid-thought, name the stopping point: "This was still open."

Don't re-open settled questions. The trust-building moment is showing you remember what they figured out.

### Reference

Response-level guidance appears in the top-level `presentation_guidance` field. The posture is about what drew the user's attention and how it connects to their own thinking.

Lead with what they chose to save — the act of capture is the signal. Bridge to their own words when the connection is genuine. Notice capture patterns over time.

## Using Contributing Catalysts

The `top_contributing_catalysts` field shows which questions drove results. Each has:
- `text`: the catalyst question
- `entity`: which entity it belongs to
- `contribution_count`: how many results it surfaced
- `presentation_guidance`: (explore register only) profile-specific framing instructions

Use these to frame **why** content surfaced: "This came up because your vault has language around X" or "The catalyst asking about Y pulled this in — it's a different angle than you might expect."

Don't list the catalysts mechanically. Weave them into your reading of the results.

## Connecting Across Results

When multiple results touch the same tension from different angles, notice that. Don't just list results sequentially.

Instead of:
> "Result 1 is about X. Result 2 is about Y. Result 3 is about Z."

Try:
> "Two of these come at the same question from opposite directions — one from your notes on [topic A], another from [topic B]. The first treats it as a problem to solve; the second seems to sit with it."

Look for:
- The same word appearing in different contexts
- Contradictions between entries (one confident, one uncertain)
- Time gaps — the same idea revisited months apart with different framing
- Unexpected connections across domains (a work note and a reading highlight touching the same nerve)

## Cross-Index Connections

If search results span multiple indexes (main vault, Readwise, applied external content), notice when threads cross boundaries. A journal entry and a book highlight touching the same tension is worth calling out — but be selective. Only flag cross-index connections when the bridge is genuine, not forced.

## Follow-up

After presenting results, suggest specific next searches based on what surfaced. Use catalyst language rather than generic terms.

Good:
- "The word 'reserves' keeps showing up — want to pull on that thread specifically?"
- "There's a tension between X and Y here that your #productivity catalysts might also reach — worth checking?"

Bad:
- "Would you like to search for anything else?"
- "Let me know if you want to explore further."

Suggest 1-2 concrete next directions. Use language from the results themselves — words the user actually wrote — as seeds for follow-up searches.

## When Nothing Surfaces

If results are sparse or low-relevance:
- The vault may not have content in this area yet. Say so directly.
- Suggest trying a different framing — abstract queries sometimes miss when the vault uses concrete, personal language. Offer a more grounded alternative.
- The vault auto-refreshes before each prompt, so results reflect current vault state.
- Don't apologize. Absence of results is information too — it tells you something about where the vault's attention has and hasn't been.
