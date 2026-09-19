#!/usr/bin/env python3
"""Sync the packaged Hermes/OpenClaw skill from the runtime skill template.

`plugin/agent-runtime-skill.md` is the canonical runtime instruction body used by
`enzyme install <runtime>`. `plugin/agent/SKILL.md` needs different frontmatter for
the Hermes/OpenClaw skill repo, but its markdown body should stay identical to the
runtime template to avoid drift.
"""

from __future__ import annotations

import argparse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUNTIME_SKILL = ROOT / "plugin" / "agent-runtime-skill.md"
PACKAGED_SKILL = ROOT / "plugin" / "agent" / "SKILL.md"

RUNTIME_BOUNDARY_ANCHORS = [
    "it is the sole\nsource of truth for scan interpretation",
    "Do not improvise a setup procedure from this runtime skill",
    "bundled `references/knowledge-practice-review.md`",
]

RUNTIME_BOUNDARY_BANNED_TOKENS = [
    "A recurring review",
    "offer an optional recurring review",
    "create or manage scheduled tasks",
]

PACKAGED_FRONTMATTER = """---
name: enzyme
description: >
  Use Enzyme in an already activated Obsidian, Markdown, or Hermes workspace
  for working-memory retrieval, source-grounded exploration, semantic search by
  concept rather than keyword, runtime note-writing, and refresh. If the
  workspace is not initialized, follow the first-time setup section before
  normal retrieval.
license: MIT
compatibility: Requires shell access (macOS arm64/x86_64, Linux x86_64/arm64). Install the enzyme CLI if it is not on PATH.
allowed-tools: Bash Read Glob Grep
metadata: { "openclaw": { "always": true, "os": ["darwin", "linux"], "primaryEnv": "OPENAI_API_KEY", "requires": { "anyBins": ["enzyme"] }, "install": [{ "id": "curl", "kind": "download", "url": "https://raw.githubusercontent.com/useenzyme/enzyme/main/install.sh", "bins": ["enzyme"], "label": "Install enzyme (curl)" }] }, "author": "jshph", "version": "0.6.0", "homepage": "https://memory.enzyme.garden" }
---
"""


def markdown_body(path: Path) -> str:
    text = path.read_text()
    if not text.startswith("---\n"):
        raise SystemExit(f"{path} does not start with YAML frontmatter")
    try:
        _, _, body = text.split("---", 2)
    except ValueError as exc:
        raise SystemExit(f"{path} has malformed YAML frontmatter") from exc
    return body.lstrip("\n").rstrip() + "\n"


def rendered_packaged_skill() -> str:
    return PACKAGED_FRONTMATTER + "\n" + markdown_body(RUNTIME_SKILL)


def validate_runtime_boundary() -> None:
    text = RUNTIME_SKILL.read_text()
    errors = [
        f"runtime setup-routing anchor missing: {anchor!r}"
        for anchor in RUNTIME_BOUNDARY_ANCHORS
        if anchor not in text
    ]
    errors.extend(
        f"runtime skill duplicates setup closing policy: {token!r}"
        for token in RUNTIME_BOUNDARY_BANNED_TOKENS
        if token in text
    )
    if errors:
        raise SystemExit("\n".join(errors))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if the packaged skill is out of sync")
    args = parser.parse_args()

    validate_runtime_boundary()
    rendered = rendered_packaged_skill()
    if args.check:
        current = PACKAGED_SKILL.read_text() if PACKAGED_SKILL.exists() else ""
        if current != rendered:
            raise SystemExit(f"{PACKAGED_SKILL} is out of sync with {RUNTIME_SKILL}; run {Path(__file__).name}")
        print(f"{PACKAGED_SKILL} is in sync")
        return

    PACKAGED_SKILL.write_text(rendered)
    print(f"synced {PACKAGED_SKILL} from {RUNTIME_SKILL}")


if __name__ == "__main__":
    main()
