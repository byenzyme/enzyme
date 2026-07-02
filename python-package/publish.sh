#!/usr/bin/env bash
# Publish enzyme-python-package to PyPI.
# Run from the enzyme-python-package directory after bumping version.
#
# Prerequisites:
#   - ~/.pypirc with PyPI token
#   - uv tool install twine build (or pip install twine build)
#
# Usage:
#   ./publish.sh          # build + upload
#   ./publish.sh --dry    # build only, skip upload

set -euo pipefail

cd "$(dirname "$0")"

VERSION=$(grep 'version' pyproject.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "Publishing enzyme-python-package v${VERSION}"

# Clean previous builds
rm -rf dist/ build/ *.egg-info

# Build
echo "Building..."
pyproject-build --quiet

# Upload (unless --dry)
if [[ "${1:-}" == "--dry" ]]; then
    echo "Dry run — skipping upload. Built:"
    ls dist/
else
    echo "Uploading to PyPI..."
    twine upload dist/*
    echo "Published: https://pypi.org/project/enzyme-python-package/${VERSION}/"
fi
