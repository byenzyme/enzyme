#!/usr/bin/env python3
"""Sync Aside's generic workspace-setup skill mirror from enzyme-rust.

`plugin/skills/enzyme-workspace-setup/SKILL.md` is the canonical source of truth.
The vendored copy at `aside/desktop/src-tauri/resources/skills/enzyme-workspace-setup/SKILL.md`
must be byte-identical. Margins-specific setup language belongs in Aside's separate
`margins-workspace-setup` wrapper, not in the generic Enzyme skill.

In default mode: writes the vendored copy.
In --check mode: asserts byte-identity, all content anchors, and no banned tokens.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CANONICAL = ROOT / "plugin" / "skills" / "enzyme-workspace-setup" / "SKILL.md"
# The aside repo is expected as a sibling checkout of enzyme-rust; override
# with ASIDE_ROOT. When it is absent (CI, other machines), aside-side
# assertions are skipped and only the canonical skill text is validated.
ASIDE_ROOT = Path(os.environ.get("ASIDE_ROOT", ROOT.parent / "aside"))
VENDORED = (
    ASIDE_ROOT / "desktop" / "src-tauri" / "resources" / "skills"
    / "enzyme-workspace-setup" / "SKILL.md"
)

MARGINS_WRAPPER = (
    ASIDE_ROOT / "desktop" / "src-tauri" / "resources" / "skills"
    / "margins-workspace-setup" / "SKILL.md"
)

# The Claude Code plugin ships the same skill; kept as a byte-identical mirror
# so the plugin never drifts from the canonical source.
PLUGIN_VENDORED = (
    ROOT / "plugin" / "plugins" / "enzyme" / "skills"
    / "enzyme-workspace-setup" / "SKILL.md"
)

# §6 content anchors — every string must appear in the canonical skill text.
CONTENT_ANCHORS = [
    "Never delete `~/.enzyme/auth.json`",
    "ENZYME_BIN",
    '"$ENZYME_BIN" --version',
    '"$ENZYME_BIN" -p "<vault path>" init --quiet',
    "--use-env-llm",
    "Do not inspect, print, unset, or rely on API-key environment variables",
    "Already usable",
    "Weakly indexable",
    "Would improve retrieval",
    "Can wait",
    "mktemp",
    "temporary directory",
    "revert.sh",
    "No body edits",
    "No restructuring needed; your vault already compounds well",
    "One Decision",
    "Ground truth is Enzyme's view",
]

# Banned raw tokens — must NOT appear anywhere in the skill text.
BANNED_TOKENS = [
    "Margins",
    "margins",
    "malformed-frontmatter",
    "csv-headers",
    "[object Object]",
    "slow interpretive pass",
    "fast search handles",
    "Credential safety",
    "No hooks",
    "enzyme-petri",
    "MARGINS_BIN",
    ".margins",
    "manifest.json schema",
    "Never print API key values",
    "OPENAI_API_KEY",
    "curl -fsSL",
    "curl -sSL",
    "command -v enzyme",
    "/opt/homebrew/bin/enzyme",
    ".local/bin/enzyme",
    "projects add",
    "install Enzyme directly",
]


def markdown_body(path: Path) -> str:
    """Extract the markdown body (after frontmatter) from a skill file.

    Strips the leading HTML sync comment line if present so the canonical body
    can be compared cleanly against the vendored body.
    """
    text = path.read_text()
    if not text.startswith("---\n"):
        raise SystemExit(f"{path} does not start with YAML frontmatter")
    try:
        _, _, body = text.split("---", 2)
    except ValueError as exc:
        raise SystemExit(f"{path} has malformed YAML frontmatter") from exc
    body = body.lstrip("\n")
    # Strip the sync comment line if present (vendored copy only).
    comment_prefix = "<!-- SYNCED FROM"
    if body.startswith(comment_prefix):
        first_newline = body.index("\n")
        body = body[first_newline + 1:].lstrip("\n")
    return body.rstrip() + "\n"


# Tokens that indicate the wrapper is duplicating the generic skill's core
# contract instead of binding Margins context to it.
WRAPPER_DUPLICATION_TOKENS = [
    "Already usable",
    "Weakly indexable",
    "Would improve retrieval",
    "revert.sh",
    "manifest.json",
    "pre.shasum",
]


def check(args: argparse.Namespace) -> None:
    """Run all --check assertions; exit non-zero on any failure."""
    errors: list[str] = []
    aside_present = ASIDE_ROOT.is_dir()

    # 1. Byte-identity: vendored generic skill == canonical generic skill.
    canonical_text = CANONICAL.read_text()
    if not aside_present:
        print(
            f"check-workspace-skill: aside repo not found at {ASIDE_ROOT}; "
            "skipping vendored-copy and wrapper assertions",
            file=sys.stderr,
        )
    elif VENDORED.exists():
        vendored_text = VENDORED.read_text()
        if vendored_text != canonical_text:
            errors.append(
                f"FAIL byte-identity: {VENDORED} differs from {CANONICAL}.\n"
                f"  Run: python3 scripts/sync-workspace-setup-skill.py"
            )
    else:
        errors.append(
            f"FAIL byte-identity: {VENDORED} does not exist.\n"
            f"  Run: python3 scripts/sync-workspace-setup-skill.py"
        )

    # 1b. Byte-identity: Claude plugin copy == canonical (same repo, always checked).
    if PLUGIN_VENDORED.exists():
        if PLUGIN_VENDORED.read_text() != canonical_text:
            errors.append(
                f"FAIL byte-identity: {PLUGIN_VENDORED} differs from {CANONICAL}.\n"
                f"  Run: python3 scripts/sync-workspace-setup-skill.py"
            )
    else:
        errors.append(
            f"FAIL byte-identity: {PLUGIN_VENDORED} does not exist.\n"
            f"  Run: python3 scripts/sync-workspace-setup-skill.py"
        )

    # 2. Content anchors: every anchor must appear in the canonical text.
    missing_anchors = [a for a in CONTENT_ANCHORS if a not in canonical_text]
    if missing_anchors:
        for anchor in missing_anchors:
            errors.append(f"FAIL content-anchor missing: {anchor!r}")

    # 3. Banned-token grep: none of the banned tokens may appear.
    for token in BANNED_TOKENS:
        if token in canonical_text:
            errors.append(f"FAIL banned-token found: {token!r}")

    # 4. Aside wrapper exists, owns Margins-specific language, and does not
    #    restate the generic skill's procedure.
    if aside_present:
        if not MARGINS_WRAPPER.exists():
            errors.append(f"FAIL missing Margins wrapper: {MARGINS_WRAPPER}")
        else:
            wrapper_text = MARGINS_WRAPPER.read_text()
            for token in ["Margins Workspace Setup", "Setup with agent", ".margins/skills/"]:
                if token not in wrapper_text:
                    errors.append(f"FAIL wrapper anchor missing: {token!r}")
            for token in WRAPPER_DUPLICATION_TOKENS:
                if token in wrapper_text:
                    errors.append(
                        f"FAIL wrapper duplicates generic-skill contract: {token!r} "
                        "(bind context in the wrapper; keep procedure in the generic skill)"
                    )

    if errors:
        print("\n".join(errors), file=sys.stderr)
        raise SystemExit(1)

    print(f"check-workspace-skill: all assertions passed ({CANONICAL})")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "fail if vendored copy is out of sync, any content anchor is missing,"
            " or any banned token is present"
        ),
    )
    args = parser.parse_args()

    if args.check:
        check(args)
        return

    PLUGIN_VENDORED.parent.mkdir(parents=True, exist_ok=True)
    PLUGIN_VENDORED.write_text(CANONICAL.read_text())
    print(f"synced {PLUGIN_VENDORED} from {CANONICAL}")

    if not ASIDE_ROOT.is_dir():
        raise SystemExit(
            f"aside repo not found at {ASIDE_ROOT}; set ASIDE_ROOT to sync the vendored copy"
        )
    VENDORED.parent.mkdir(parents=True, exist_ok=True)
    VENDORED.write_text(CANONICAL.read_text())
    print(f"synced {VENDORED} from {CANONICAL}")


if __name__ == "__main__":
    main()
