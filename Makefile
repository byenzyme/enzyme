ENZYME_HOME := $(HOME)/.enzyme
MODELS_DIR := $(ENZYME_HOME)/models/distilled

.PHONY: install-model install check-model

# Copy distilled model files to ~/.enzyme/models/distilled/
install-model:
	@mkdir -p $(MODELS_DIR)
	@cp models/distilled/model.safetensors $(MODELS_DIR)/model.safetensors
	@cp models/distilled/tokenizer.json $(MODELS_DIR)/tokenizer.json
	@cp models/distilled/config.json $(MODELS_DIR)/config.json
	@echo "Installed model to $(MODELS_DIR)"
	@ls -lh $(MODELS_DIR)/model.safetensors $(MODELS_DIR)/tokenizer.json $(MODELS_DIR)/config.json

# Build release binary and install to cargo bin
install: install-model
	cargo install --path crates/enzyme-cli

# Verify model files exist in ~/.enzyme
check-model:
	@test -f $(MODELS_DIR)/model.safetensors && test -f $(MODELS_DIR)/tokenizer.json && test -f $(MODELS_DIR)/config.json \
		&& echo "Model OK: $$(ls -lh $(MODELS_DIR)/model.safetensors | awk '{print $$5}')" \
		|| (echo "Model missing. Run: make install-model" && exit 1)
