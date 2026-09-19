#!/usr/bin/env python3
"""Sync the public repo's README from this repo's canonical one.

The release workflow's `sync-plugin` job rsyncs `plugin/` onto the root of
`byenzyme/enzyme`, so `plugin/README.md` — not this repo's root `README.md` —
is what the public repo actually shows. Keeping two hand-edited copies is how
they drifted: the public README described a concept graph and listed catalyst
profiles by three deprecated aliases long after the root README moved on.

Root `README.md` is canonical. This mirrors it to `plugin/README.md` so the
next release publishes it. Run with `--check` in CI to fail on drift.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CANONICAL = ROOT / "README.md"
MIRROR = ROOT / "plugin" / "README.md"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit non-zero if the mirror is out of date instead of writing it",
    )
    args = parser.parse_args()

    canonical = CANONICAL.read_text(encoding="utf-8")
    mirror = MIRROR.read_text(encoding="utf-8") if MIRROR.exists() else None

    if canonical == mirror:
        return 0

    if args.check:
        rel_canonical = CANONICAL.relative_to(ROOT)
        rel_mirror = MIRROR.relative_to(ROOT)
        print(
            f"{rel_mirror} is out of date with {rel_canonical}.\n"
            f"The public repo publishes {rel_mirror}, so this drift would ship.\n"
            f"Run: python3 scripts/sync-public-readme.py",
            file=sys.stderr,
        )
        return 1

    MIRROR.write_text(canonical, encoding="utf-8")
    print(f"synced {MIRROR.relative_to(ROOT)} from {CANONICAL.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
