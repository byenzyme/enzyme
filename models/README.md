# Embedding Model

**Model**: [MongoDB/mdbr-leaf-ir](https://huggingface.co/MongoDB/mdbr-leaf-ir) (23M params, 384d)
**Format**: ONNX int4 weight-only quantized (`MatMulNBitsQuantizer`, block_size=32)
**Size**: ~52 MB (down from 87 MB FP32)
**Tokenizer**: HuggingFace `tokenizer.json` with `padding=Fixed(128)`

## Files

| File | Tracked | Description |
|------|---------|-------------|
| `model.onnx` | No (gitignored, 52 MB) | Int4 quantized ONNX model |
| `tokenizer.json` | Yes (695 KB) | Tokenizer config with padding/truncation |

## Restoring ~/.enzyme

If `~/.enzyme/` gets wiped, restore from this directory:

```bash
make install-model
```

Or manually:

```bash
mkdir -p ~/.enzyme/models
cp models/model.onnx ~/.enzyme/models/
cp models/tokenizer.json ~/.enzyme/models/
```

## Re-downloading model.onnx

If `models/model.onnx` is missing from the repo (fresh clone), regenerate it from the
HuggingFace source using the conversion script in enzyme-python:

```bash
cd ../enzyme-python
python scripts/convert_leaf_ir.py --format onnx --validate
cp src/models/embeddings_onnx/model.onnx ../enzyme-rust/models/
```

Or extract from enzyme-python git history:

```bash
cd ../enzyme-python
git show fb35cea:src/models/embeddings_onnx/model.onnx > ../enzyme-rust/models/model.onnx
```

## Provenance

- **Source model**: `MongoDB/mdbr-leaf-ir-asym` from HuggingFace
- **Quantization**: onnxruntime `MatMulNBitsQuantizer` (int4, symmetric, block_size=32)
- **Conversion script**: `enzyme-python/scripts/convert_leaf_ir.py`
- **Conversion commit**: `fb35cea` in enzyme-python ("chore: update embedding models")
- **Architecture**: identical to `all-MiniLM-L6-v2` (6-layer BERT, 384 hidden, 12 heads)
