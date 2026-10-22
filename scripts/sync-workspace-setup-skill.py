#!/usr/bin/env python3
"""Check Enzyme's setup surfaces and sync its neutral review contract.

Enzyme owns the operational setup skill. Its bundled knowledge-practice review
reference is intentionally command-free and may be composed into other product
adapters. Margins mirrors the neutral reference and
keeps its own commands, state model, and mutation policy.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CANONICAL_DIR = ROOT / "skills" / "enzyme-workspace-setup"
CANONICAL_SKILL = CANONICAL_DIR / "SKILL.md"
CANONICAL_REVIEW = CANONICAL_DIR / "references" / "knowledge-practice-review.md"

# The Margins checkout is normally a sibling named aside-desktop. CI and other
# machines may omit it; set ASIDE_ROOT to check or update another checkout.
ASIDE_ROOT = Path(os.environ.get("ASIDE_ROOT", ROOT.parent / "aside-desktop"))
MARGINS_REVIEW = (
    ASIDE_ROOT
    / "crates"
    / "public"
    / "margins-workflows"
    / "resources"
    / "guidance"
    / "knowledge-practice-review.md"
)

# These check command/distribution and recovery contracts, not a particular
# narration, tier vocabulary, or heading structure. User comprehension is tested
# by independent rollout readers, not required slogans.
SKILL_ANCHORS = [
    "ENZYME_BIN",
    '"$ENZYME_BIN" --version',
    '"$ENZYME_BIN" -p "<vault path>" model status',
    "compile --packet",
    '"$ENZYME_BIN" spec compile',
    "AGENTS.md",
    '"$ENZYME_BIN" -p "<vault path>" init --quiet',
    "doctor --json",
    "petri --query",
    "catalyze",
    "--use-env-llm",
    "runtime installation has already",
    "save_to",
    "mktemp",
    "sidecar/",
    "manifest.json",
    "pre.shasum",
    "revert.sh",
    "No body edits",
    "original mtimes",
    "init --force",
    "Choose what can keep returning",
    "references/knowledge-practice-review.md",
]

SKILL_BANNED_TOKENS = [
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
    "curl -fsSL",
    "curl -sSL",
    "command -v enzyme",
    "/opt/homebrew/bin/enzyme",
    ".local/bin/enzyme",
    "projects add",
    "install Enzyme directly",
    "When you <something you already do>",
    "When the user already writes regularly",
    "bring one thread like this back every Monday",
]

REVIEW_ANCHORS = [
    "# Knowledge Practice Review Contract",
    "Read evidence as evidence",
    "Form a recognizable account",
    "Prefer meaningful continuity over structural noise",
    "Help the user keep hold of one important thread",
    "End insight-first",
    "Choose what can keep returning",
    "When you <something you already do>",
    "old notes do not need",
    "two to four plain-language",
    "operational receipt last",
]

# The shared layer must remain safe to compose into either product. These tokens
# indicate that operational ownership leaked into the neutral judgment module.
REVIEW_BANNED_TOKENS = [
    "Enzyme",
    "Margins",
    "enzyme ",
    "margins ",
    ".enzyme",
    ".margins",
    "scan.v2",
    "workspace plan",
    "workspace apply",
    "init --quiet",
    "revert.sh",
    "config.toml",
]

def assert_mirror(errors: list[str], canonical: Path, mirror: Path) -> None:
    if not mirror.exists():
        errors.append(f"FAIL missing mirror: {mirror}")
    elif mirror.read_bytes() != canonical.read_bytes():
        errors.append(
            f"FAIL byte-identity: {mirror} differs from {canonical}.\n"
            "  Run: python3 scripts/sync-workspace-setup-skill.py"
        )


def check() -> None:
    errors: list[str] = []
    skill_text = CANONICAL_SKILL.read_text()
    review_text = CANONICAL_REVIEW.read_text()

    if ASIDE_ROOT.is_dir():
        assert_mirror(errors, CANONICAL_REVIEW, MARGINS_REVIEW)
    else:
        print(
            f"check-workspace-skill: Margins repo not found at {ASIDE_ROOT}; "
            "skipping the downstream neutral-reference assertion",
            file=sys.stderr,
        )

    for anchor in SKILL_ANCHORS:
        if anchor not in skill_text:
            errors.append(f"FAIL skill content-anchor missing: {anchor!r}")
    for token in SKILL_BANNED_TOKENS:
        if token in skill_text:
            errors.append(f"FAIL skill banned-token found: {token!r}")

    for anchor in REVIEW_ANCHORS:
        if anchor not in review_text:
            errors.append(f"FAIL review content-anchor missing: {anchor!r}")
    for token in REVIEW_BANNED_TOKENS:
        if token in review_text:
            errors.append(f"FAIL review operational token found: {token!r}")

    if errors:
        print("\n".join(errors), file=sys.stderr)
        raise SystemExit(1)

    print(
        "check-workspace-skill: canonical setup and shared review contract are in sync"
    )


def sync_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(source.read_bytes())
    print(f"synced {destination} from {source}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail on drift or boundary leaks")
    args = parser.parse_args()

    if args.check:
        check()
        return

    if not ASIDE_ROOT.is_dir():
        raise SystemExit(
            f"Margins repo not found at {ASIDE_ROOT}; set ASIDE_ROOT to sync its reference"
        )
    sync_file(CANONICAL_REVIEW, MARGINS_REVIEW)


if __name__ == "__main__":
    main()
