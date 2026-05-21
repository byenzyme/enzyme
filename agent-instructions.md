<!-- enzyme:start -->
## Enzyme Workspace Context

This workspace uses Enzyme for local semantic retrieval over markdown. Run `enzyme` commands from this workspace root.

Use the installed Enzyme skill for operational details. The skill contains the setup workflow, retrieval workflow, note-writing guidance, and presentation rules. This `AGENTS.md` section is intentionally small so workspace instructions stay readable.

If `.enzyme/enzyme.db` is missing, initialize Enzyme with the skill workflow: scan the workspace, preserve existing Obsidian/markdown structure, validate `~/.enzyme/config.toml`, run `enzyme init --quiet`, then run `enzyme install` for the active runtime.

Retrieval defaults:
- Start broad sessions with `enzyme petri` or `enzyme petri --query "user prompt"`.
- Use catalyst phrases from petri to compose `enzyme catalyze "query"` searches.
- Use exact search only for names, tags, wikilinks, and literal text.

Structure policy: do not impose a new memory schema. Treat existing folders, inboxes, daily notes, people/contact pages, tags, wikilinks, and frontmatter dates/entity fields as retrieval signal. Propose frontmatter, people-page, or folder backfills only with user confirmation.
<!-- enzyme:end -->
