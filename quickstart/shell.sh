#!/usr/bin/env bash
# Open a shell (or run a one-shot command) inside the task container
# as uid 1000 — the same uid + environment the benchmark's agents ran in.
#
# Usage:
#   ./shell.sh                        # interactive shell
#   ./shell.sh cargo build --release --lib --target wasm32-unknown-unknown
#   ./shell.sh bash -lc 'oracle info'
#   ./shell.sh my-agent-cli --task /task/TASK.md
#
# Requires: `docker compose up -d` from this directory first.

set -euo pipefail
cd "$(dirname "$0")"

SERVICE="task"

if ! docker compose ps --services --filter "status=running" 2>/dev/null | grep -qx "$SERVICE"; then
    echo "error: '$SERVICE' service is not running." >&2
    echo "bring it up first:" >&2
    echo "  cd $(pwd) && docker compose up -d" >&2
    exit 1
fi

if [ $# -eq 0 ]; then
    exec docker compose exec "$SERVICE" bash -l
fi

exec docker compose exec "$SERVICE" "$@"
