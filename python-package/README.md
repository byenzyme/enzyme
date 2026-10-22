# enzyme-python-package

Pip-installable wrapper for the [enzyme](https://github.com/byenzyme/enzyme) binary. Auto-downloads the correct platform binary and embedding model on first run.

## Install

```bash
pip install enzyme-python-package
```

First run downloads the enzyme binary (~8 MB) and embedding model (~23 MB) to `~/.local/bin/enzyme`.

## Usage

```bash
enzyme petri                    # vault orientation
enzyme catalyze "query"         # semantic search
enzyme init                     # initialize a vault
enzyme refresh                  # re-index
enzyme status                   # vault stats
```

All commands proxy directly to the enzyme binary.

## Release flow

This package should be published alongside enzyme binary releases. When a new enzyme version is tagged in [byenzyme/enzyme](https://github.com/byenzyme/enzyme):

1. Update `VERSION` in `enzyme_python_package/__init__.py` to match the new tag
2. Update `version` in `pyproject.toml`
3. Run `./publish.sh`

Requires `~/.pypirc` with a PyPI token and `twine`/`build` installed (`uv tool install twine build`).

```bash
# Full publish
./publish.sh

# Dry run (build only)
./publish.sh --dry
```

## Supported platforms

- macOS arm64
- macOS x86_64
- Linux x86_64
- Linux arm64
