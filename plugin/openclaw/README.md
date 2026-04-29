# Enzyme — OpenClaw plugin

Semantic search and pattern discovery for Obsidian vaults, exposed as an OpenClaw tool plugin.

## Install

The plugin shells out to the `enzyme` CLI binary. Install that first:

```bash
curl -fsSL https://enzyme.garden/install.sh | bash
# or: brew install jshph/enzyme/enzyme-cli
```

Then install the plugin:

```bash
openclaw plugins install @jshph/enzyme-openclaw
```

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
- **`agent_end`** — when `autoRefresh: true`, runs `enzyme refresh --quiet` after each turn.

## Caveats

- Auto-recall adds ~50–200ms per turn (8ms petri + spawn overhead).
- `autoRefresh` requires `hooks.allowConversationAccess: true`.
- This plugin does **not** claim the `kind: "memory"` slot, so it composes with `memory-core` and other memory plugins.
