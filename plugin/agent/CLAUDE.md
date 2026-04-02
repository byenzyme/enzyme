# enzyme-skill

## Do not edit files here directly

Most files in this repo are synced from [enzyme-rust](https://github.com/jshph/enzyme-rust) on every release. Edits made here will be overwritten.

**To make changes, edit the source in enzyme-rust:**

| This repo | Source (enzyme-rust) |
|---|---|
| `SKILL.md` | `plugin/agent/SKILL.md` |
| `plugin.yaml` | `plugin/agent/plugin.yaml` |
| `*.py` | `plugin/agent/*.py` |
| `install.sh` | `plugin/agent/install.sh` |
| `references/*.md` | `plugin/agent/*.md` (petri-guide, search-guide) |
| `scripts/setup.sh` | `plugin/agent/scripts/setup.sh` |
| `README.md` | `plugin/agent/README.md` |

**Files managed by CI only (not from plugin/agent/):**
- `bin/*` — platform binaries, extracted from build artifacts
- `models/*` — embedding model files, downloaded from release assets
- `.gitattributes` — Git LFS tracking rules, repo-level
