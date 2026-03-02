ENZYME_HOME := $(HOME)/.enzyme
MODELS_DIR := $(ENZYME_HOME)/models

.PHONY: install-model install check-model

# Copy model files to ~/.enzyme/models/
install-model:
	@mkdir -p $(MODELS_DIR)
	@cp models/model.onnx $(MODELS_DIR)/model.onnx
	@cp models/tokenizer.json $(MODELS_DIR)/tokenizer.json
	@echo "Installed model to $(MODELS_DIR)"
	@ls -lh $(MODELS_DIR)/model.onnx $(MODELS_DIR)/tokenizer.json

# Build release binary and install to cargo bin
install: install-model
	cargo install --path crates/enzyme-cli

# Verify model files exist in ~/.enzyme
check-model:
	@test -f $(MODELS_DIR)/model.onnx && test -f $(MODELS_DIR)/tokenizer.json \
		&& echo "Model OK: $$(ls -lh $(MODELS_DIR)/model.onnx | awk '{print $$5}')" \
		|| (echo "Model missing. Run: make install-model" && exit 1)
