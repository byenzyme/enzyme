"""Tool handlers for the enzyme Hermes plugin.

Each handler shells out to the enzyme CLI and returns a JSON string.
Signature: (args: dict, **kwargs) -> str
"""

import json
import os
import subprocess


def _ensure_initialized(timeout: int = 120) -> None:
    """Run enzyme init if the vault has no index yet."""
    if not os.path.exists(os.path.join(".enzyme", "enzyme.db")):
        subprocess.run(
            ["enzyme", "init"],
            capture_output=True,
            timeout=timeout,
        )


def _run_enzyme(args: list[str], timeout: int = 30) -> str:
    """Run an enzyme CLI command and return JSON string."""
    try:
        result = subprocess.run(
            ["enzyme"] + args,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            return json.dumps({"error": result.stderr.strip() or f"enzyme exited with code {result.returncode}"})
        output = result.stdout.strip()
        if not output:
            return json.dumps({"ok": True})
        return json.dumps({"output": output})
    except subprocess.TimeoutExpired:
        return json.dumps({"error": f"enzyme timed out after {timeout}s"})
    except FileNotFoundError:
        return json.dumps({"error": "enzyme binary not found. Run install.sh first."})


def handle_petri(args: dict, **kwargs) -> str:
    _ensure_initialized()
    cmd = ["petri", "-n", str(args.get("top", 10))]
    query = args.get("query")
    if query:
        cmd.extend(["--query", query])
    return _run_enzyme(cmd)


def handle_catalyze(args: dict, **kwargs) -> str:
    _ensure_initialized()
    query = args.get("query", "")
    cmd = ["catalyze", query, "-n", str(args.get("limit", 10))]
    register = args.get("register", "explore")
    if register != "explore":
        cmd.extend(["--register", register])
    return _run_enzyme(cmd)


def handle_refresh(args: dict, **kwargs) -> str:
    cmd = ["refresh", "--quiet"]
    return _run_enzyme(cmd, timeout=120)


def handle_status(args: dict, **kwargs) -> str:
    return _run_enzyme(["status"])


def handle_init(args: dict, **kwargs) -> str:
    cmd = ["init"]
    if args.get("quiet", True):
        cmd.append("--quiet")
    guide = args.get("guide")
    if guide:
        cmd.extend(["--guide", guide])
    return _run_enzyme(cmd, timeout=120)
