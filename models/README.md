# Embedding Model

**Model**: [minishlab/potion-base-8M](https://huggingface.co/minishlab/potion-base-8M) (distilled via model2vec)
**Format**: SafeTensors (model2vec distilled — static token embeddings, no transformer inference)
**Size**: ~29 MB
**Tokenizer**: HuggingFace `tokenizer.json`

## Files

| File | Location | Description |
|------|----------|-------------|
| `model.safetensors` | `distilled/` | Distilled static embedding weights |
| `tokenizer.json` | `distilled/` | Tokenizer config |
| `config.json` | `distilled/` | Model config |

## Restoring ~/.enzyme

If `~/.enzyme/` gets wiped, restore via the CLI:

```bash
enzyme setup
```

Or manually:

```bash
mkdir -p ~/.enzyme/models/distilled
cp models/distilled/model.safetensors ~/.enzyme/models/distilled/
cp models/distilled/tokenizer.json ~/.enzyme/models/distilled/
cp models/distilled/config.json ~/.enzyme/models/distilled/
```

## Provenance

- **Source model**: `minishlab/potion-base-8M` from HuggingFace
- **Technique**: model2vec distillation — replaces transformer inference with static token embedding lookup
- **Previous model**: `MongoDB/mdbr-leaf-ir` (ONNX, rten backend) — superseded by model2vec for faster inference and smaller binary
