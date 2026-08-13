# Catalyst 135M v3

`catalyst-135m-v3` is Enzyme's small on-device model for generating retrieval
catalysts from a user's own Markdown vault. It is a task-specific SmolLM2-135M
fine-tune, distributed as an IQ4_NL GGUF with an in-domain importance matrix.

## Intended use

The model is intended to run inside Enzyme's grammar-constrained local engine.
It generates short questions and tensions that Enzyme embeds for semantic
retrieval. It is not a general-purpose assistant or factual reference model.

Enzyme applies a constrained JSON grammar, a compact local prompt, an 8,192
token context window, and bounded per-entity context. Using the checkpoint
without those serving constraints can produce malformed or low-quality output.

## Provenance and privacy

The checkpoint was trained for catalyst generation from pseudonymized examples.
The training dataset is not distributed with the weights. The public checkpoint
was cleared after the project's pseudonym review.

## Base model and license

The checkpoint derives from SmolLM2-135M. Users must comply with the base
model's license and notices. Enzyme's source code remains licensed separately
under the repository license.

## Limitations

- Generated catalysts can be awkward, repetitive, or misleading.
- The model should not be used for factual, medical, legal, or safety-critical advice.
- Output quality depends on the selected vault excerpts and does not imply that
  the model understands the complete vault.
- Generated text can reflect biases or errors present in the base model and
  fine-tuning examples.

## Artifact

- File: `enzyme-catalyst-135m-v3.gguf`
- Size: 91,727,072 bytes
- SHA-256: `710a79eea9a84b22cc18b8626b9d893e1a5a2de61f8852fcef839099e8138eab`
- Enzyme install name: `catalyst-135m-v3`

Install with:

```bash
enzyme model install catalyst-135m-v3
```
