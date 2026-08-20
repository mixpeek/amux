.PHONY: install run dev check test clean status restart

BIN_DIR   ?= $(HOME)/.local/bin
PORT      ?= 8824
LABEL     := com.amux.server-rs

# First-time or upgrade: build, install, load launchd, wait for /health.
install:
	./install.sh

# Rebuild release + reinstall binary; launchd restarts the server automatically
# (the server watches its own binary mtime and exits for launchd to relaunch).
run:
	cargo build --release -p amux-server
	install -m 0755 target/release/amux-server $(BIN_DIR)/amux-server-rs
	@echo "Installed. The running server self-adopts within ~5s."
	@sleep 3
	@curl -sk https://localhost:$(PORT)/health | python3 -m json.tool 2>/dev/null \
		|| echo "Server not responding yet — check: make status"

# Run against a scratch DB for local development (no migration risk to live data).
dev:
	AMUX_DB=/tmp/amux-dev.db AMUX_RS_PORT=$(PORT) cargo run -p amux-server

# Syntax + type checks (fast, no link).
check:
	cargo check --workspace
	@for f in crates/amux-dashboard/static/*.js; do \
		node --check "$$f" 2>/dev/null && echo "  ✓ $$f" || echo "  ✗ $$f"; \
	done

# Run the test suite.
test:
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test -p amux-server

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
