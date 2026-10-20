# Picard build tasks. Run `make` for the list.
#
# Signing: builds are ad-hoc signed ("-"), which is enough to run on this Mac.
# To sign with a Developer ID instead:
#   make dmg APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
# `security find-identity -v -p codesigning` lists the identities you have.

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

TAURI_DIR := src-tauri
CARGO := cargo
CARGO_FLAGS := --manifest-path $(TAURI_DIR)/Cargo.toml
APPLE_SIGNING_IDENTITY ?= -
export APPLE_SIGNING_IDENTITY

RELEASE_BUNDLE := $(TAURI_DIR)/target/release/bundle
APP := $(RELEASE_BUNDLE)/macos/Picard.app

.DEFAULT_GOAL := help

.PHONY: help
help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-10s %s\n", $$1, $$2}'

.PHONY: setup
setup: ## Install JavaScript dependencies
	pnpm install

.PHONY: dev
dev: ## Run Picard from source, rebuilding on change
	pnpm tauri dev

.PHONY: fmt
fmt: ## Format the Rust code
	$(CARGO) fmt $(CARGO_FLAGS)

.PHONY: lint
lint: ## rustfmt check, clippy (warnings are errors), svelte-check
	$(CARGO) fmt $(CARGO_FLAGS) --check
	$(CARGO) clippy $(CARGO_FLAGS) --all-targets -- -D warnings
	pnpm -s check

.PHONY: test
test: ## Run the Rust and JavaScript tests
	$(CARGO) test $(CARGO_FLAGS)
	pnpm -s test

.PHONY: test-all
test-all: test ## Also run the disk image test (attaches and detaches a 200 MB image)
	$(CARGO) test $(CARGO_FLAGS) -- --ignored

.PHONY: check
check: lint test ## Everything that should pass before a commit

.PHONY: scan
scan: ## Show what Picard would remove with your config (changes nothing)
	$(CARGO) run $(CARGO_FLAGS) --quiet -- scan

.PHONY: app
app: ## Quick debug build of Picard.app
	pnpm tauri build --debug --bundles app
	@echo "Built $(TAURI_DIR)/target/debug/bundle/macos/Picard.app"

.PHONY: dmg
dmg: check ## Release Picard.app and .dmg for Apple Silicon (runs check first)
	$(MAKE) bundle

# The release workflow runs `check` in its own step, then this one with the
# signing secrets, so the test dependencies never see them.
.PHONY: bundle
bundle: ## Release Picard.app and .dmg without running check
	pnpm tauri build --bundles app,dmg
	@echo "Built:"; ls -1d $(APP) $(RELEASE_BUNDLE)/dmg/*.dmg

.PHONY: verify
verify: ## Check the release build's signature and the .dmg contents
	codesign --verify --deep --strict --verbose=1 $(APP)
	codesign --display --verbose=2 $(APP) 2>&1 | grep -E '^(Identifier|Authority|Signature|TeamIdentifier)='
	@dmg=$$(ls -1t $(RELEASE_BUNDLE)/dmg/*.dmg | head -1); \
	mnt=$$(mktemp -d); \
	dev=$$(diskutil image attach --readOnly --mountOptions nobrowse --mountPoint "$$mnt" "$$dmg" | awk '/^\/dev\/disk[0-9]+[[:space:]]/ {print $$1; exit}'); \
	trap 'diskutil eject "$$dev" >/dev/null; rmdir "$$mnt"' EXIT; \
	echo "$$dmg contains:"; ls -1 "$$mnt"; \
	test -d "$$mnt/Picard.app" && test -L "$$mnt/Applications" && echo "OK: Picard.app and an Applications link"

.PHONY: clean
clean: ## Remove build output
	$(CARGO) clean $(CARGO_FLAGS)
	rm -rf dist
