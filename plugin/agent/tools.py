"""Tool handlers for the enzyme Hermes plugin.

Each handler shells out to the enzyme CLI and returns a JSON string.
Signature: (args: dict, **kwargs) -> str
"""

import json
import os
import subprocess


def _vault_is_initialized() -> bool:
    """Check if the current directory has an enzyme index."""
    return os.path.exists(os.path.join(".enzyme", "enzyme.db"))


def _not_initialized_error() -> str:
    return json.dumps(
        {
            "error": "enzyme vault is not initialized",
            "next": "Load the installed `enzyme-workspace-setup` skill and follow it end to end. If that skill is missing, run `enzyme install hermes --source local` once, then read its SKILL.md and bundled knowledge-practice review reference before issuing setup commands.",
        }
    )


def _run_enzyme(args: list[str], timeout: int = 30) -> str:
    """Run an enzyme CLI command and return JSON string."""
    try:
        result = subprocess.run(
            ["enzyme"] + args,
            capture_output=True,
            text=True,
            stdin=subprocess.DEVNULL,
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
    if not _vault_is_initialized():
        return _not_initialized_error()
    cmd = ["petri"]
    if args.get("top") is not None:
        cmd.extend(["-n", str(args["top"])])
    query = args.get("query")
    if query:
        cmd.extend(["--query", query])
    return _run_enzyme(cmd)


def handle_catalyze(args: dict, **kwargs) -> str:
    if not _vault_is_initialized():
        return _not_initialized_error()
    query = args.get("query", "")
    cmd = ["catalyze", query, "-n", str(args.get("limit", 10))]
    target = args.get("target")
    if target:
        cmd.extend(["--target", target])
    register = args.get("register", "explore")
    if register != "explore":
        cmd.extend(["--register", register])
    timeout = 300 if target else 30
    return _run_enzyme(cmd, timeout=timeout)


def handle_refresh(args: dict, **kwargs) -> str:
    cmd = ["refresh", "--quiet"]
    return _run_enzyme(cmd, timeout=120)


def handle_scan(args: dict, **kwargs) -> str:
    cmd = ["scan"]
    if args.get("write_config", False):
        cmd.append("--write-config")
    return _run_enzyme(cmd, timeout=120)


def handle_status(args: dict, **kwargs) -> str:
    return _run_enzyme(["status"])


def handle_init(args: dict, **kwargs) -> str:
    cmd = ["init"]
    if args.get("quiet", True):
        cmd.append("--quiet")
    return _run_enzyme(cmd, timeout=120)
