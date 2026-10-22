#!/usr/bin/env python3
"""Smoke test the Hermes plugin source layout without a Hermes install.

This catches the failures that made the nested plugin copy confusing:
- plugin files must live at agent root
- legacy setup.py must not be imported or shipped
- register(ctx) must wire expected tools and hooks

It intentionally does not run lifecycle hooks, because those may install a
bundled binary into ~/.local/bin during a real Hermes session.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PLUGIN_DIR = ROOT / "agent"


class FakeContext:
    def __init__(self) -> None:
        self.hooks: dict[str, object] = {}
        self.tools: dict[str, object] = {}

    def register_hook(self, name: str, handler: object) -> None:
        self.hooks[name] = handler

    def register_tool(self, *, name: str, handler: object, **kwargs: object) -> None:
        self.tools[name] = handler


def load_plugin():
    spec = importlib.util.spec_from_file_location(
        "enzyme_agent_plugin",
        PLUGIN_DIR / "__init__.py",
        submodule_search_locations=[str(PLUGIN_DIR)],
    )
    if spec is None or spec.loader is None:
        raise AssertionError("failed to create import spec for agent")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def assert_layout() -> None:
    required = [
        "plugin.yaml",
        "__init__.py",
        "hooks.py",
        "schemas.py",
        "tools.py",
        "SKILL.md",
    ]
    for rel in required:
        path = PLUGIN_DIR / rel
        if not path.exists():
            raise AssertionError(f"missing required plugin file: {path}")

    forbidden = [
        "setup.py",
        "enzyme",
    ]
    for rel in forbidden:
        path = PLUGIN_DIR / rel
        if path.exists():
            raise AssertionError(f"legacy nested/bootstrap path should not exist: {path}")

    for py_file in ["__init__.py", "hooks.py"]:
        text = (PLUGIN_DIR / py_file).read_text()
        if "import setup" in text or "from . import setup" in text:
            raise AssertionError(f"{py_file} still imports legacy setup module")


def assert_registration() -> None:
    plugin = load_plugin()
    ctx = FakeContext()
    plugin.register(ctx)

    expected_hooks = {"on_session_start", "pre_llm_call", "on_session_end"}
    missing_hooks = expected_hooks - set(ctx.hooks)
    if missing_hooks:
        raise AssertionError(f"missing hooks: {sorted(missing_hooks)}")

    expected_tools = {
        "enzyme_petri",
        "enzyme_catalyze",
        "enzyme_refresh",
        "enzyme_status",
        "enzyme_init",
    }
    missing_tools = expected_tools - set(ctx.tools)
    if missing_tools:
        raise AssertionError(f"missing tools: {sorted(missing_tools)}")


def main() -> int:
    assert_layout()
    assert_registration()
    print("Hermes plugin smoke test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
