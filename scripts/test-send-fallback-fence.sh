#!/usr/bin/env bash
# Raw terminal fallback was removed after the TubeScience stacked-paste incident.
# Its replacement must preserve retry identity and never inject after a timeout.
set -euo pipefail
exec python3 "$(dirname "$0")/test-send-retry-identity.py"
