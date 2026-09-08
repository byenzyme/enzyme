.PHONY: install build-native install-native native-cpu sync-agent-skill check-agent-skill sync-workspace-skill check-workspace-skill bench-cpu-portable bench-cpu-native bench-cpu asm-ese-native release-check release-check-agentic snapshot-vault

# Build release binary and install to cargo bin
install:
	scripts/cargo-lane shared -- cargo install --path crates/enzyme-cli

# Local-only optimized release build for benchmarking/development.
# Do not ship this binary: target-cpu=native may emit instructions that fail on older CPUs.
build-native:
	RUSTFLAGS="-Ctarget-cpu=native" scripts/cargo-lane shared -- cargo build --release --bin enzyme
	@echo "Built native-optimized binary at $$(scripts/cargo-lane target-dir)/release/enzyme"

# Install a local-only native-optimized enzyme into cargo bin.
install-native:
	RUSTFLAGS="-Ctarget-cpu=native" scripts/cargo-lane shared -- cargo install --path crates/enzyme-cli --force

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

# Check the canonical setup skill and thin marketplace bootstrap, and mirror the
# neutral practice-review reference into Margins.
sync-workspace-skill:
	python3 scripts/sync-workspace-setup-skill.py

check-workspace-skill:
	python3 scripts/sync-workspace-setup-skill.py --check

# Deterministic release checks: install matrix, sync contract, vault lifecycle.
# Set ENZYME_TEST_VAULT to a snapshot-vault.sh output to add the real-scale
# tier; set ENZYME_CHECK_FULL=0 to skip the hosted-bootstrap full-init tier.
release-check:
	scripts/cargo-lane shared -- cargo build -p enzyme-cli --features local-llm
	ENZYME_BIN="$${ENZYME_BIN:-$$(scripts/cargo-lane target-dir)/debug/enzyme}" bash scripts/release-checks/10-install-matrix.sh
	ENZYME_BIN="$${ENZYME_BIN:-$$(scripts/cargo-lane target-dir)/debug/enzyme}" bash scripts/release-checks/20-sync-contract.sh
	ENZYME_BIN="$${ENZYME_BIN:-$$(scripts/cargo-lane target-dir)/debug/enzyme}" bash scripts/release-checks/30-vault-lifecycle.sh

# Agentic release checks: a headless Claude agent does the memory.enzyme.garden
# cold-start install and a full teardown, graded by deterministic assertions.
# Uses the local claude subscription; run at release time or nightly.
release-check-agentic:
	scripts/cargo-lane shared -- cargo build -p enzyme-cli --features local-llm
	ENZYME_BIN="$${ENZYME_BIN:-$$(scripts/cargo-lane target-dir)/debug/enzyme}" bash scripts/release-checks/40-agentic-setup.sh

# Snapshot the real vault's markdown corpus into ~/.enzyme-test-vaults/real
# (local only; never enters git).
snapshot-vault:
	bash scripts/release-checks/snapshot-vault.sh

# Microbenchmark ESE encoding and 512-dim cosine loops with portable CPU settings.
bench-cpu-portable:
	scripts/cargo-lane disposable -- cargo run --release -p enzyme-core --example bench_cpu_features -- 4096 100

# Microbenchmark ESE encoding and 512-dim cosine loops with target-cpu=native.
bench-cpu-native:
	scripts/cargo-lane disposable -- env RUSTFLAGS="-Ctarget-cpu=native" cargo run --release -p enzyme-core --example bench_cpu_features -- 4096 100

# Run portable then native for a quick before/after comparison.
bench-cpu: bench-cpu-portable bench-cpu-native

# Emit assembly for a tiny ESE benchmark and grep for SIMD/FMA-ish instructions.
# x86: look for vfmadd*, vmulps/vaddps, ymm/zmm. ARM64: look for fmla/fmadd and vector .4s ops.
asm-ese-native:
	scripts/cargo-lane disposable -- bash -c 'RUSTFLAGS="-Ctarget-cpu=native" cargo rustc -p enzyme-core --release --example bench_cpu_features -- --emit=asm && find "$$CARGO_TARGET_DIR/release/examples" -name "*.s" -print -exec grep -E "\b(vfmadd|vmulps|vaddps|ymm|zmm|fmla|fmadd|fadd\.4s|fmul\.4s|\.4s)\b" {} \; | head -120'
