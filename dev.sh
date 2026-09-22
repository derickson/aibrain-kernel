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
cargo run --manifest-path rust/Cargo.toml -- serve --watch &
pids+=($!)

# The Python server refuses to start until /health answers, so give the Rust
# side a moment rather than racing it into a failure message.
sleep 2

echo "==> aibrain (ui)"
python3 -m aibrain "$@" &
pids+=($!)

wait -n
