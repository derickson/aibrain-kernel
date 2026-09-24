#!/usr/bin/env bash
# Everything the app needs, in one terminal.
#
# Three processes now: Postgres in a container, the Rust service that owns the
# corpus, and the Python server that serves the UI. Ctrl-C stops the two we
# started; the database keeps running, because it is cheap and stopping it
# would throw away the page cache on every restart.

set -euo pipefail
cd "$(dirname "$0")"

DB_URL="${AIBRAIN_DATABASE_URL:-postgres://aibrain:aibrain@127.0.0.1:5433/aibrain}"
export AIBRAIN_DATABASE_URL="$DB_URL"

# Everything below is shown live in this terminal AND streamed, timestamped,
# to a log file — so an agent (or you) can review a run after the fact. `tee`
# passes the terminal copy through untouched (so the "waiting..." dots still
# animate live) and only timestamps the branch written to the log file. Plain
# `date` per line rather than awk's strftime, which isn't universally built in.
mkdir -p logs
LOG_FILE="logs/dev-$(date +%Y%m%d-%H%M%S).log"
timestamp() {
  while IFS= read -r line; do
    printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$line"
  done
}
exec > >(tee >(timestamp >> "$LOG_FILE")) 2>&1
echo "==> logging to $LOG_FILE"

compose() {
  if docker compose version >/dev/null 2>&1; then docker compose "$@"
  else docker-compose "$@"; fi
}

echo "==> postgres"
compose up -d db

echo -n "    waiting for the database"
for _ in $(seq 1 60); do
  if compose exec -T db pg_isready -U aibrain -q 2>/dev/null; then
    echo " — ready"
    break
  fi
  echo -n "."
  sleep 1
done

pids=()
stop() {
  echo
  echo "==> stopping"
  for pid in "${pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap stop INT TERM EXIT

echo "==> aibrain-core"
stdbuf -oL -eL cargo run --manifest-path rust/Cargo.toml -- serve --watch &
pids+=($!)

# The Python server refuses to start until /health answers. On a first-time
# build cargo must compile everything — poll instead of sleeping a fixed amount.
echo -n "    waiting for aibrain-core"
for _ in $(seq 1 180); do
  if curl -sf http://127.0.0.1:8781/health >/dev/null 2>&1; then
    echo " — ready"
    break
  fi
  echo -n "."
  sleep 2
done

echo "==> aibrain (ui)"
stdbuf -oL -eL python3 -m aibrain "$@" &
pids+=($!)

# wait -n requires bash 4.3+; macOS ships bash 3.2. Poll instead.
while true; do
  for pid in "${pids[@]}"; do
    kill -0 "$pid" 2>/dev/null || exit 0
  done
  sleep 2
done
