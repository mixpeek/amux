.PHONY: install install-cli run dev check test clean status restart

BIN_DIR   ?= $(HOME)/.local/bin
PORT      ?= 8824
LABEL     := com.amux.server-rs
export CARGO_TARGET_DIR ?= $(HOME)/.amux/rust-build-target

# First-time or upgrade: build, install, load launchd, wait for /health.
install:
	./install.sh

# Publish only syntax-checked Bash CLI bytes; no Cargo build or server restart.
install-cli:
	./scripts/install-cli.sh "$(BIN_DIR)"

# Rebuild release + reinstall binary; launchd restarts the server automatically
# (the server watches its own binary mtime and exits for launchd to relaunch).
run:
	AMUX_RS_INSTALL="$(BIN_DIR)/amux-server-rs" ./scripts/rust-auto-build.sh
	@echo "Build/deployment result: ~/.amux/logs/rust-auto-build.log"
	@sleep 3
	@curl -sk https://localhost:$(PORT)/health | python3 -m json.tool 2>/dev/null \
		|| echo "Server not responding yet — check: make status"

# Run against a scratch DB for local development (no migration risk to live data).
dev:
	./scripts/safe-cargo.sh build -p amux-server
	AMUX_DB=/tmp/amux-dev.db AMUX_RS_PORT=$(PORT) "$(CARGO_TARGET_DIR)/debug/amux-server"

# Syntax + type checks (fast, no link).
check:
	./scripts/safe-cargo.sh check --workspace
	@for f in crates/amux-dashboard/static/*.js; do \
		node --check "$$f" 2>/dev/null && echo "  ✓ $$f" || echo "  ✗ $$f"; \
	done

# Run the test suite.
test:
	./scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings
	./scripts/test-contended.sh -p amux-server

# Server health + launchd status.
status:
	@echo "=== launchd ==="
	@launchctl list $(LABEL) 2>/dev/null || echo "$(LABEL) not loaded"
	@echo ""
	@echo "=== /health ==="
	@curl -sk https://localhost:$(PORT)/health 2>/dev/null | python3 -m json.tool \
		|| echo "Server not responding on port $(PORT)"

# Restart the launchd-managed server.
restart:
	launchctl kickstart -k gui/$$(id -u)/$(LABEL)
	@sleep 2
	@curl -sk https://localhost:$(PORT)/health | python3 -m json.tool 2>/dev/null \
		|| echo "Server not responding yet"
