<!-- enzyme:start -->
## Enzyme Workspace Context

This workspace uses Enzyme for local semantic retrieval over markdown. Run `enzyme` commands from this workspace root.

Use the installed Enzyme skill for operational details. The skill contains the setup workflow, retrieval workflow, note-writing guidance, and presentation rules. This Enzyme workspace context section is intentionally small so workspace instructions stay readable.

If `.enzyme/enzyme.db` is missing, initialize Enzyme with the skill workflow: confirm the Enzyme CLI is already installed, scan the workspace, audit existing markdown structure, show a setup preview, create/tune `~/.enzyme/config.toml` before init, run `enzyme init --quiet`, then simulate one useful prompt with petri/catalyze.

Retrieval defaults:
- Start broad sessions with `enzyme petri` or `enzyme petri --query "user prompt"` to recognize active/relevant ideas before answering.
- Use catalyst phrases from petri to compose `enzyme catalyze "query"` searches that activate those ideas as source-grounded connections.
- Use exact search only for names, tags, wikilinks, and literal text.
- For external refs, run `enzyme apply ./target-dir`, then compare `enzyme catalyze "query"` with `enzyme catalyze "query" --target ./target-dir`.

Structure policy: do not impose a new memory schema. Treat existing folders, inboxes, daily notes, people/contact pages, tags, wikilinks, and frontmatter dates/entity fields as retrieval signal. Enzyme should free natural capture, not require a parallel agent memory tree. Propose frontmatter, people-page, folder, or apply-target changes only with user confirmation.
<!-- enzyme:end -->
