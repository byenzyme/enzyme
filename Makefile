.PHONY: install build-native install-native native-cpu sync-agent-skill check-agent-skill sync-workspace-skill check-workspace-skill bench-cpu-portable bench-cpu-native bench-cpu asm-ese-native

# Build release binary and install to cargo bin
install:
	cargo install --path crates/enzyme-cli

# Local-only optimized release build for benchmarking/development.
# Do not ship this binary: target-cpu=native may emit instructions that fail on older CPUs.
build-native:
	RUSTFLAGS="-Ctarget-cpu=native" cargo build --release --bin enzyme
	@echo "Built native-optimized binary at target/release/enzyme"

# Install a local-only native-optimized enzyme into cargo bin.
install-native:
	RUSTFLAGS="-Ctarget-cpu=native" cargo install --path crates/enzyme-cli --force

# Show what rustc maps target-cpu=native to on this machine.
native-cpu:
	@rustc -vV | grep '^host:'
	@rustc --print target-cpus | grep -E '^    native|^    apple-|^    x86-64' | head -20

# Keep the packaged Hermes/OpenClaw skill body in sync with the runtime skill
# template used by `enzyme install <runtime>`.
sync-agent-skill:
	python3 scripts/sync-agent-runtime-skill.py

check-agent-skill:
	python3 scripts/sync-agent-runtime-skill.py --check

# Keep the vendored aside workspace-setup skill in sync with the canonical
# enzyme-rust source at plugin/skills/enzyme-workspace-setup/SKILL.md.
sync-workspace-skill:
	python3 scripts/sync-workspace-setup-skill.py

check-workspace-skill:
	python3 scripts/sync-workspace-setup-skill.py --check

# Microbenchmark ESE encoding and 512-dim cosine loops with portable CPU settings.
bench-cpu-portable:
	CARGO_TARGET_DIR=target/bench-portable cargo run --release -p enzyme-core --example bench_cpu_features -- 4096 100

# Microbenchmark ESE encoding and 512-dim cosine loops with target-cpu=native.
bench-cpu-native:
	CARGO_TARGET_DIR=target/bench-native RUSTFLAGS="-Ctarget-cpu=native" cargo run --release -p enzyme-core --example bench_cpu_features -- 4096 100

# Run portable then native for a quick before/after comparison.
bench-cpu: bench-cpu-portable bench-cpu-native

# Emit assembly for a tiny ESE benchmark and grep for SIMD/FMA-ish instructions.
# x86: look for vfmadd*, vmulps/vaddps, ymm/zmm. ARM64: look for fmla/fmadd and vector .4s ops.
asm-ese-native:
	CARGO_TARGET_DIR=target/asm-ese-native RUSTFLAGS="-Ctarget-cpu=native" cargo rustc -p enzyme-core --release --example bench_cpu_features -- --emit=asm
	@find target/asm-ese-native/release/examples -name '*.s' -print -exec grep -E '\b(vfmadd|vmulps|vaddps|ymm|zmm|fmla|fmadd|fadd\.4s|fmul\.4s|\.4s)\b' {} \; | head -120
