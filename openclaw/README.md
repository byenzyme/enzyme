# Enzyme — OpenClaw plugin

Semantic search and pattern discovery for Obsidian vaults, exposed as an OpenClaw tool plugin.

## Install

The plugin shells out to the `enzyme` CLI binary. Install that first:

```bash
curl -fsSL https://memory.enzyme.garden/install.sh | bash
# or: brew install useenzyme/enzyme/enzyme-cli
```

Then install the plugin:

```bash
openclaw plugins install @jshph/enzyme-openclaw
```

Then activate Enzyme for the workspace you want OpenClaw to use:

```bash
cd /path/to/your/vault
enzyme install openclaw
```

`enzyme install openclaw` writes the small workspace marker into `AGENTS.md` and installs the full runtime skill into `~/.openclaw/skills/enzyme/SKILL.md` or `$OPENCLAW_HOME/skills/enzyme/SKILL.md`. The plugin handles automatic recall and optional refresh; the skill tells OpenClaw how to interpret Enzyme's retrieval results and preserve the user's existing markdown structure.

On first use, ask OpenClaw to set up Enzyme for the vault. It should inspect the markdown structure, run `enzyme scan`, confirm the setup stance, persist config with `enzyme scan --write-config`, validate `~/.enzyme/config.toml`, and run `enzyme init --quiet`. For terminal-only setup, run those commands manually before starting OpenClaw.

For local development (`--link` mode), OpenClaw rejects symlinked deps that resolve outside the plugin root. Use a packed tarball of the sibling bridge package:

```bash
cd plugin/openclaw-bridge && npm pack --pack-destination /tmp
cd ../openclaw && rm -rf node_modules package-lock.json
npm install /tmp/enzyme-openclaw-bridge-*.tgz
openclaw plugins install $(pwd) --link
```

Configure your vault in `~/.openclaw/openclaw.json`:

```json5
{
  plugins: {
    entries: {
      enzyme: {
        enabled: true,
        config: {
          vaultPath: "/Users/you/obsidian",
          autoRecall: true,
          autoRefresh: false
        },
        hooks: {
          // Required only when autoRefresh is enabled.
          allowConversationAccess: true
        }
      }
    }
  }
}
```

## Tools

| Tool | What it does |
|---|---|
| `enzyme_petri` | Working memory: top entities + catalyst phrases. Run on first message. |
| `enzyme_catalyze` | Semantic search by concept. Compose queries from petri catalysts. |
| `enzyme_status` | Vault index stats. |

## Hooks

- **`before_prompt_build`** — when `autoRecall: true` (default), `enzyme petri --query <user msg>` runs before each turn and prepends the result as system context.
- **`agent_end`** — when `autoRefresh: true`, runs `enzyme refresh --quiet` so markdown written near the end of a turn/session becomes retrievable.

## Caveats

- Auto-recall adds ~50–200ms per turn (8ms petri + spawn overhead).
- `autoRefresh` requires `hooks.allowConversationAccess: true`.
- This plugin does **not** claim the `kind: "memory"` slot, so it composes with `memory-core` and other memory plugins.
