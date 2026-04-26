#!/usr/bin/env sh

while IFS= read -r model; do
    uv run --with ../../target/wheels/ese_py-*.whl bench.py "$model"
done < models.txt
