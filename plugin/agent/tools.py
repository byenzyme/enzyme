"""Tool handlers for the enzyme Hermes plugin.

Each handler shells out to the enzyme CLI and returns JSON.
No reimplementation of core logic — the binary does all the work.
"""

import subprocess
from typing import Any


def _run_enzyme(args: list[str], timeout: int = 30) -> dict[str, Any]:
    """Run an enzyme CLI command and return the result."""
    try:
        result = subprocess.run(
            ["enzyme"] + args,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            return {"error": result.stderr.strip() or f"enzyme exited with code {result.returncode}"}
        output = result.stdout.strip()
        if not output:
            return {"ok": True}
        # enzyme outputs JSON — return as string for the LLM to read
        return {"output": output}
    except subprocess.TimeoutExpired:
        return {"error": f"enzyme timed out after {timeout}s"}
    except FileNotFoundError:
        return {"error": "enzyme binary not found. Run install.sh first."}


def handle_petri(query: str | None = None, top: int = 10) -> dict[str, Any]:
    args = ["petri", "-n", str(top)]
    if query:
        args.extend(["--query", query])
    return _run_enzyme(args)


def handle_catalyze(
    query: str,
    limit: int = 10,
    register: str = "explore",
) -> dict[str, Any]:
    args = ["catalyze", query, "-n", str(limit)]
    if register != "explore":
        args.extend(["--register", register])
    return _run_enzyme(args)


def handle_refresh(full: bool = False) -> dict[str, Any]:
    args = ["refresh", "--quiet"]
    if full:
        args.append("--full")
    return _run_enzyme(args, timeout=120)


def handle_status() -> dict[str, Any]:
    return _run_enzyme(["status"])


def handle_init(guide: str | None = None, quiet: bool = True) -> dict[str, Any]:
    args = ["init"]
    if quiet:
        args.append("--quiet")
    if guide:
        args.extend(["--guide", guide])
    return _run_enzyme(args, timeout=120)
