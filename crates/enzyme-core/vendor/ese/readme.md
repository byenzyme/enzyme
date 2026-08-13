## Much faster text embedding preview

This repo contains a rust crate containing the static embedding model deployment pipeline described [here](https://www.flowercomputer.com/news/fast-static-embedding/). 

The crate exposes two functions, `encode` and `encode_single`, take a look in `lib.rs` for more info.

### `encode`
This accepts an array of strings, encoding them in parallel

### `encode_single`
This accepts a single string

## To use

Clone the repo and reference it locally as a crate in your Rust project. Enzyme keeps the optional Python wrapper outside the subtree-ready core at `scaffolding/ese-api-py/`.

### Crate features

#### Quantization and Truncation
By default this crate provides an unquantized model truncated to 512 dimensions. To change this, change the feature setting in your cargo.toml for your `ese` dependency ([see `Cargo.toml` for all crate features](./Cargo.toml)).
