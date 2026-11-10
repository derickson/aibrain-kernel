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

# Loopback only, unless started as `make remotedev` (AIBRAIN_REMOTEDEV=1).
# The UI checks every request's Host against loopback names plus
# AIBRAIN_ALLOWED_HOSTS; that list is cleared here so a variable left exported
# in your shell can never open a plain `make dev` to the network. Remote mode
# binds the UI — never aibrain-core, which stays on 127.0.0.1 — to one LAN
# address and allows exactly that address.
ui_args=()
remote_ip=""
if [[ "${AIBRAIN_REMOTEDEV:-}" == "1" ]]; then
  remote_ip="${REMOTE_HOST:-}"
  if [[ -z "$remote_ip" ]]; then
    for iface in en0 en1; do
      remote_ip="$(ipconfig getifaddr "$iface" 2>/dev/null || true)"
      [[ -n "$remote_ip" ]] && break
    done
  fi
  if [[ -z "$remote_ip" ]] && command -v hostname >/dev/null; then
    remote_ip="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
  fi
  if [[ -z "$remote_ip" ]]; then
    echo "!! could not find a LAN address; pass one: make remotedev REMOTE_HOST=192.168.x.y" >&2
    exit 1
  fi
  export AIBRAIN_ALLOWED_HOSTS="$remote_ip"
  ui_args=(--host "$remote_ip")
else
  unset AIBRAIN_ALLOWED_HOSTS
fi

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
if [[ -n "$remote_ip" ]]; then
  echo "    REMOTE DEV: the UI answers on http://$remote_ip:8760/ to anyone on this network."
  echo "    They can edit notes and configure agents (which run commands here)."
  echo "    Use it on a network you trust, and stop it when you are done."
fi
stdbuf -oL -eL python3 -m aibrain ${ui_args[@]+"${ui_args[@]}"} "$@" &
pids+=($!)

# wait -n requires bash 4.3+; macOS ships bash 3.2. Poll instead.
while true; do
  for pid in "${pids[@]}"; do
    kill -0 "$pid" 2>/dev/null || exit 0
  done
  sleep 2
done
