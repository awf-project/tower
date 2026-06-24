# tower — developer task runner.
# Mirrors the quality gate in docs/development.md (run in the same order as CI).
#
# Since the extension-system migration (spec 20) the engine is a single static
# binary with out-of-process *native* sidecar extensions (extensions/*). There is
# no WASM build step, no WASI SDK, and no wasm fixtures. Build workspace binaries
# before tests so the host can locate sidecars under target/debug/.

BIN          := tower
PKG          := core_engine
VERSION      := v$(shell sed -n 's/^version = "\(.*\)"/\1/p' crates/$(PKG)/Cargo.toml | head -1)
HOST_TARGET  := $(shell rustc -vV | sed -n 's/^host: //p')
INSTALL_DIR  ?= $(HOME)/.local/bin

# `install-extensions` knobs.
#   EXTENSIONS  — reference extensions to install (space-separated; manifest dir names).
#   EXT_DEST    — discovery scope to install into. Default: local project scope.
#                 For the global XDG scope, pass
#                   EXT_DEST=$(HOME)/.local/share/tower/extensions
#   EXT_PROFILE — release (default, deployable) or debug (fast, reuses dev build).
EXTENSIONS   ?= ast debug fmt lint lsp
EXT_DEST     ?= .tower/extensions
EXT_PROFILE  ?= release

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
build: ## Build the workspace (debug) — host + native extensions
	cargo build --workspace

.PHONY: release
release: ## Build the tower binary (release)
	cargo build --release -p $(PKG)

.PHONY: run
run: ## Run the MCP server over stdio (cargo run)
	cargo run -p $(PKG)

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
test: ## Run the workspace test suite (compiles native extensions first)
	cargo build --workspace --bins
	cargo test --workspace

.PHONY: deny
deny: ## License & advisory policy
	cargo deny check

.PHONY: gate
gate: fmt-check clippy test deny ## Full quality gate, same sequence as CI

## ---------------------------------------------------------------------------
## Distribution
## ---------------------------------------------------------------------------
# Produces the exact assets scripts/install.sh downloads:
#   dist/$(BIN)-$(VERSION)-$(HOST_TARGET).tar.gz  (+ .sha256)
#
# The tarball bundles the host binary AND the reference extensions ($(EXTENSIONS)),
# so a fresh install has AST/LSP/fmt without a source checkout. Layout:
#   ./$(BIN)
#   ./extensions/<name>/extension.toml
#   ./extensions/<name>/<name>_extension   (manifest `command` is dir-relative)
# scripts/install.sh lays the extensions/ tree into the global discovery scope.
.PHONY: dist
dist: release ## Package a release tarball (host + reference extensions) + sha256
	cargo build --release $(addprefix -p ,$(addsuffix _extension,$(EXTENSIONS)))
	@mkdir -p dist
	@archive="$(BIN)-$(VERSION)-$(HOST_TARGET).tar.gz"; \
	staging="$$(mktemp -d)"; \
	install -m 0755 target/release/$(BIN) "$$staging/$(BIN)"; \
	for ext in $(EXTENSIONS); do \
	  mkdir -p "$$staging/extensions/$$ext"; \
	  install -m 0644 "extensions/$$ext/extension.toml" "$$staging/extensions/$$ext/extension.toml"; \
	  install -m 0755 "target/release/$${ext}_extension" "$$staging/extensions/$$ext/$${ext}_extension"; \
	done; \
	tar -czf "dist/$$archive" -C "$$staging" .; \
	rm -rf "$$staging"; \
	( cd dist && (sha256sum "$$archive" 2>/dev/null || shasum -a 256 "$$archive") > "$$archive.sha256" ); \
	echo "dist/$$archive"; \
	echo "dist/$$archive.sha256"

.PHONY: install
install: release ## Build release and install tower to $(INSTALL_DIR)
	@mkdir -p "$(INSTALL_DIR)"
	install -m 0755 target/release/$(BIN) "$(INSTALL_DIR)/$(BIN)"
	@echo "installed $(BIN) -> $(INSTALL_DIR)/$(BIN)"

# Reference extensions are discovered, not bundled: each must live in a scope as
# <scope>/<name>/extension.toml alongside its native binary (the manifest's
# `command` is resolved relative to the extension directory). This builds them
# and lays out that structure. Restart tower (or reconnect the MCP server) after.
.PHONY: install-extensions
install-extensions: ## Build + install reference extensions ($(EXTENSIONS), $(EXT_PROFILE)) into $(EXT_DEST)
	cargo build $(if $(filter release,$(EXT_PROFILE)),--release,) \
	  $(addprefix -p ,$(addsuffix _extension,$(EXTENSIONS)))
	@for ext in $(EXTENSIONS); do \
	  dest="$(EXT_DEST)/$$ext"; \
	  mkdir -p "$$dest"; \
	  install -m 0644 "extensions/$$ext/extension.toml" "$$dest/extension.toml"; \
	  install -m 0755 "target/$(EXT_PROFILE)/$${ext}_extension" "$$dest/$${ext}_extension"; \
	  echo "installed extension '$$ext' ($(EXT_PROFILE)) -> $$dest"; \
	done
	@echo "restart tower / reconnect the MCP server to load the new extensions"

## ---------------------------------------------------------------------------
## Housekeeping
## ---------------------------------------------------------------------------
.PHONY: clean
clean: ## Remove build + dist artifacts
	cargo clean
	rm -rf dist
