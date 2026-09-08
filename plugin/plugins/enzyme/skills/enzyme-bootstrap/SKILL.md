---
name: enzyme-bootstrap
description: Connect Enzyme to the current Markdown vault. Use when the user asks to install, set up, or connect Enzyme and the vault does not yet have durable Enzyme agent instructions. Installs the full runtime and workspace skills into the vault, then hands setup to them. Do not use for ordinary retrieval in a vault that is already connected.
allowed-tools: Read, Glob, Bash
---

# Connect Enzyme To This Vault

This skill is only the doorway. Its job is to leave the vault able to help future
agents—not to carry the full setup procedure inside the marketplace plugin.

Resolve the vault root and the active agent runtime (`codex` or `claude`). Check
that Markdown notes actually live in the proposed vault before writing anything.

If the vault already contains both of the following for the active runtime, do
not reinstall them:

- the `enzyme` runtime skill;
- the `enzyme-workspace-setup` skill and its
  `references/knowledge-practice-review.md` file.

Read the installed workspace setup skill and its reference completely, then
follow them. Their instructions are authoritative.

Otherwise, check `enzyme --version`. If the command is missing, explain that the
Enzyme CLI is required and offer its current official installation route. Do not
silently download or install a binary.

Before installing vault files, say plainly what will happen and ask permission:

> I can add Enzyme's durable instructions to this vault. That lets future chats
> and scheduled reviews know how to refresh it and reconnect recent ideas with
> older ones. This adds agent instruction files; it does not edit your notes or
> initialize the index. Want me to add them here?

After a clear yes, run the matching command from the vault root:

```bash
enzyme -p "<vault path>" install codex --source local
# or, in Claude Code:
enzyme -p "<vault path>" install claude --source local
```

Use `--source local` so the installed instructions match the running Enzyme
binary. Do not run `init`, `refresh`, change model settings, or read note contents
under this permission.

Then read the newly installed `enzyme-workspace-setup/SKILL.md` and its reference
completely and continue with that skill. It owns diagnosis, privacy choices,
setup consent, initialization, proof on the user's notes, and the final handoff.
