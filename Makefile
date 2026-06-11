# tower — developer task runner.
# Mirrors the quality gate in docs/development.md (run in the same order as CI).

BIN          := tower
PKG          := core_engine
VERSION      := v$(shell sed -n 's/^version = "\(.*\)"/\1/p' crates/$(PKG)/Cargo.toml | head -1)
HOST_TARGET  := $(shell rustc -vV | sed -n 's/^host: //p')
WASM_TARGET  := wasm32-wasip1
INSTALL_DIR  ?= $(HOME)/.local/bin

# WASI toolchain for the tree-sitter C sources in plugin_ast (see AGENTS.md).
# Defaults to the tree-sitter cache; an exported CC_wasm32_wasip1 / AR_wasm32_wasip1
# (e.g. in CI) overrides these at recipe time.
WASI_CC      ?= $(HOME)/.cache/tree-sitter/wasi-sdk/bin/wasm32-wasip1-clang
WASI_AR      ?= $(HOME)/.cache/tree-sitter/wasi-sdk/bin/llvm-ar

# wasm fixtures that must be built before `cargo test` (see development.md).
WASM_FIXTURES := hello_plugin fixture_abi_mismatch fixture_panic_plugin \
                 fixture_loop_plugin fixture_loop_hook_plugin
FIXTURE_FLAGS := $(addprefix -p ,$(WASM_FIXTURES))

# Crates excluded from host-side `cargo test` (wasm-only or tested separately).
TEST_EXCLUDES := --exclude hello_plugin --exclude plugin_ast \
                 --exclude fixture_abi_mismatch --exclude fixture_panic_plugin \
                 --exclude fixture_loop_plugin --exclude fixture_loop_hook_plugin

.DEFAULT_GOAL := help

## ---------------------------------------------------------------------------
## Help
## ---------------------------------------------------------------------------
.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

## ---------------------------------------------------------------------------
## Build & run
## ---------------------------------------------------------------------------
.PHONY: build
build: ## Build host crates (debug)
	cargo build

.PHONY: release
release: ## Build the tower binary (release)
	cargo build --release -p $(PKG)

.PHONY: run
run: ## Run the MCP server over stdio (cargo run)
	cargo run -p $(PKG)

## ---------------------------------------------------------------------------
## WASM artifacts
## ---------------------------------------------------------------------------
.PHONY: wasm-fixtures
wasm-fixtures: ## Build the 5 wasm test fixtures + hello_plugin
	cargo build $(FIXTURE_FLAGS) --target $(WASM_TARGET)

.PHONY: wasm-ast
wasm-ast: ## Build plugin_ast for wasm (uses cached WASI SDK; override via CC_wasm32_wasip1 / AR_wasm32_wasip1)
	CC_wasm32_wasip1="$${CC_wasm32_wasip1:-$(WASI_CC)}" \
	AR_wasm32_wasip1="$${AR_wasm32_wasip1:-$(WASI_AR)}" \
	cargo build -p plugin_ast --target $(WASM_TARGET)

## ---------------------------------------------------------------------------
## Quality gate (same order as CI — see docs/development.md)
## ---------------------------------------------------------------------------
.PHONY: fmt
fmt: ## Format the workspace
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting (CI step 1)
	cargo fmt --all --check

.PHONY: clippy
clippy: ## Lint, warnings as errors (CI step 2)
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: test
test: wasm-fixtures wasm-ast ## Run host-side tests (builds wasm first)
	cargo test --workspace $(TEST_EXCLUDES)

.PHONY: test-ast
test-ast: ## Run plugin_ast host-side tests
	cargo test -p plugin_ast

.PHONY: deny
deny: ## License & advisory policy
	cargo deny check

.PHONY: gate
gate: fmt-check clippy wasm-fixtures wasm-ast ## Full quality gate (fmt+clippy+wasm+tests+deny)
	cargo test --workspace $(TEST_EXCLUDES)
	cargo test -p plugin_ast
	cargo deny check

## ---------------------------------------------------------------------------
## Distribution
## ---------------------------------------------------------------------------
# Produces the exact assets scripts/install.sh downloads:
#   dist/$(BIN)-$(VERSION)-$(HOST_TARGET).tar.gz  (+ .sha256)
.PHONY: dist
dist: release ## Package a release tarball + sha256 for the host target
	@mkdir -p dist
	@archive="$(BIN)-$(VERSION)-$(HOST_TARGET).tar.gz"; \
	tar -czf "dist/$$archive" -C target/release $(BIN); \
	( cd dist && (sha256sum "$$archive" 2>/dev/null || shasum -a 256 "$$archive") > "$$archive.sha256" ); \
	echo "dist/$$archive"; \
	echo "dist/$$archive.sha256"

.PHONY: install
install: release ## Build release and install tower to $(INSTALL_DIR)
	@mkdir -p "$(INSTALL_DIR)"
	install -m 0755 target/release/$(BIN) "$(INSTALL_DIR)/$(BIN)"
	@echo "installed $(BIN) -> $(INSTALL_DIR)/$(BIN)"

## ---------------------------------------------------------------------------
## Housekeeping
## ---------------------------------------------------------------------------
.PHONY: clean
clean: ## Remove build + dist artifacts
	cargo clean
	rm -rf dist
