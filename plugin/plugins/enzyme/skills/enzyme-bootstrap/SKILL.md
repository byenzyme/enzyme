---
name: enzyme-bootstrap
description: Connect Enzyme to the current Markdown vault. Use when the user asks to install, set up, or connect Enzyme and the vault does not yet have durable Enzyme agent instructions. Installs the full runtime and workspace skills into the vault, then hands setup to them. Do not use for ordinary retrieval in a vault that is already connected.
allowed-tools: Read, Glob, Bash
---

# Connect Enzyme To This Vault

This marketplace plugin is retired and will stop receiving updates. Enzyme now
installs from the command line and keeps its skills inside the vault, so the
plugin is no longer needed. This skill is only the doorway: it connects the
vault the supported way and then tells the user they can remove the plugin.

Resolve the vault root and the active agent runtime (`codex` or `claude`). Check
that Markdown notes actually live in the proposed vault before writing anything.

If the vault already contains both of the following for the active runtime, do
not reinstall them:

- the `enzyme` runtime skill;
- the `enzyme-workspace-setup` skill and its
  `references/knowledge-practice-review.md` file.

Read the installed workspace setup skill and its reference completely, then
follow them. Their instructions are authoritative.

Otherwise, check `enzyme --version`. If it fails or prints a message about
bundled plugin binaries, the Enzyme CLI is not installed. Ask permission, then
install it with the official installer:

```bash
curl -fsSL https://raw.githubusercontent.com/byenzyme/enzyme/main/install.sh | bash
```

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

Tell the user once that the plugin is retired and that the vault no longer needs
it. They can remove it with `claude plugin uninstall enzyme@enzyme` or
`codex plugin remove enzyme@enzyme`. Do not remove it for them.

Then read the newly installed `enzyme-workspace-setup/SKILL.md` and its reference
completely and continue with that skill. It owns diagnosis, privacy choices,
setup consent, initialization, proof on the user's notes, and the final handoff.
