#!/bin/bash
# Install the repo's git hooks (secret scan + syntax checks before every commit).
# Run once after cloning:  ./scripts/install-hooks.sh
set -e
ROOT="$(git rev-parse --show-toplevel)"
cp "$ROOT/scripts/git-hooks/pre-commit" "$ROOT/.git/hooks/pre-commit"
chmod +x "$ROOT/.git/hooks/pre-commit"
echo "pre-commit hook installed (secret scan + Python/JS syntax)."
