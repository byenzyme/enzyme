<!-- enzyme:start -->

## Enzyme Workspace Context

This workspace uses Enzyme for local semantic retrieval over markdown. Run `enzyme` commands from this workspace root.

Use the installed Enzyme skill for operational details. The skill contains the full setup, retrieval, and note-writing workflow. This Enzyme workspace context section keeps only durable presentation rules and workspace defaults so AGENTS.md stays readable.

If `.enzyme/enzyme.db` is missing, initialize Enzyme with the skill workflow: confirm the Enzyme CLI is already installed, scan the workspace, audit existing markdown structure, show a setup preview, create/tune `~/.enzyme/config.toml` before init, run `enzyme init --quiet`, then simulate one useful prompt with petri/catalyze.

Retrieval defaults:

- Start broad sessions with `enzyme petri` or `enzyme petri --query "user prompt"` to recognize active/relevant ideas before answering.
- Use catalyst phrases from petri to compose `enzyme catalyze "query"` searches that activate those ideas as source-grounded connections.
- Use exact search only for names, tags, wikilinks, and literal text.
- For external refs, run `enzyme apply ./target-dir`, then compare `enzyme catalyze "query"` with `enzyme catalyze "query" --target ./target-dir`.

Presentation policy:

- Use Enzyme command names internally; do not expose petri, catalyze, catalyst IDs, scores, or tool names to the user unless asked.
- Ground observations with `enzyme catalyze` excerpts. Lead with the user's words and file attribution, then add a small observation.
- For exploration, open one specific connection among the user's notes rather than presenting a topic list.
- For continuity questions, restore prior conclusions, decisions, trajectory, and stopping points before introducing new analysis.
- For reference/import material, lead with what the user chose to save, bridge to their own notes when possible, and avoid treating imported content as authoritative.
- Do not lead with metadata. Notice repeated words, time gaps, changed wording, adjacent ideas, practical consequences, or source disagreements. End with one concrete next direction.

Structure policy: do not impose a new memory schema. Treat existing folders, inboxes, daily notes, people/contact pages, tags, wikilinks, and frontmatter dates/entity fields as retrieval signal. Enzyme should free natural capture, not require a parallel agent memory tree. Propose frontmatter, people-page, folder, or apply-target changes only with user confirmation.

<!-- enzyme:end -->
