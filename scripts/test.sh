#!/usr/bin/env bash
# One command for the whole test suite: Rust unit tests, Rust integration
# tests gated on Postgres, the Python suite (which also runs the Node edge
# script), and Elasticsearch-gated Rust tests when credentials are available.
#
# Every stage that cannot run prints why and is skipped rather than failing —
# a missing container is an environment problem, not a broken build. The
# script's own exit code is non-zero if anything that did run failed.
#
#   scripts/test.sh
#
# Postgres is expected in the `aibrain-db` container (see docker-compose.yml
# or dev.sh); AIBRAIN_TEST_DATABASE_URL overrides which database the
# Postgres-gated stages use (default: aibrain_test on it, created if missing).
# Elasticsearch credentials come from the environment, or from a repo-root
# .env if AIBRAIN_TEST_DATABASE_URL/ELASTICSEARCH_* are not already set.

set -uo pipefail
cd "$(dirname "$0")/.."

# reconcile_brains() (reached from aibrain.config.Config.load) rewrites
# .claude/settings.json's deny list to match this checkout's linked vaults.
# Every test that goes through it patches config.SETTINGS_PATH to a private
# temp file except one — the StartupTests subprocess, which sets
# AIBRAIN_MANAGE_DENY_RULES=0 in its own env for exactly that reason — so
# this script does not need to (and must not: DenyRuleTests needs the real
# behaviour switched on to prove it works).

# A repo-root .env carries the real Elasticsearch credentials for local runs.
# Only loaded when the environment did not already supply them, so CI secrets
# are never shadowed by a stale file.
if [ -f .env ] && [ -z "${ELASTICSEARCH_URL:-}" ]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

DB_URL="${AIBRAIN_TEST_DATABASE_URL:-postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_test}"
DB_NAME="${DB_URL##*/}"
export AIBRAIN_TEST_DATABASE_URL="$DB_URL"

overall_failed=0
declare -a summary=() skipped=()

pass() { summary+=("PASS  $1"); }
fail() { summary+=("FAIL  $1"); overall_failed=1; }
skip() { summary+=("SKIP  $1"); skipped+=("$1: $2"); }

header() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

# ---------------------------------------------------------------------------
# Is a Postgres reachable at DB_URL, creating DB_NAME on it if the server
# answers but the database itself does not exist yet?
# ---------------------------------------------------------------------------
postgres_ready=0
check_postgres() {
  if ! command -v docker >/dev/null 2>&1; then
    return 1
  fi
  if ! docker exec aibrain-db pg_isready -U aibrain -q 2>/dev/null; then
    return 1
  fi
  docker exec aibrain-db psql -U aibrain -d aibrain -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '${DB_NAME}'" 2>/dev/null \
    | grep -q 1 \
    || docker exec aibrain-db psql -U aibrain -d aibrain -c \
      "CREATE DATABASE ${DB_NAME}" >/dev/null 2>&1
  docker exec aibrain-db psql -U aibrain -d "${DB_NAME}" -tAc "SELECT 1" 2>/dev/null | grep -q 1
}
if check_postgres; then
  postgres_ready=1
fi

es_ready=0
if [ -n "${ELASTICSEARCH_URL:-}" ] && [ -n "${ELASTICSEARCH_API_KEY:-}" ]; then
  es_ready=1
fi

# ---------------------------------------------------------------------------
# 1. Rust unit tests — Postgres and Elasticsearch both switched off, so every
#    gated test in the crate takes its own "no credentials" early return.
# ---------------------------------------------------------------------------
header "Rust unit tests"
if command -v cargo >/dev/null 2>&1; then
  if AIBRAIN_TEST_DATABASE_URL= ELASTICSEARCH_URL= ELASTICSEARCH_API_KEY= \
      cargo test --manifest-path rust/Cargo.toml; then
    pass "cargo test (unit)"
  else
    fail "cargo test (unit)"
  fi
else
  skip "cargo test (unit)" "cargo is not installed"
fi

# ---------------------------------------------------------------------------
# 2. Rust integration tests gated on Postgres (live::, todo::integration,
#    es::integration's non-ES coalescing check, and anything else that reads
#    AIBRAIN_TEST_DATABASE_URL). Elasticsearch stays off here — that is stage 4.
# ---------------------------------------------------------------------------
header "Rust integration tests (Postgres)"
if [ "$postgres_ready" -ne 1 ]; then
  skip "cargo test (Postgres integration)" \
    "no Postgres reachable at ${DB_NAME} via the aibrain-db container — start it with \`docker compose up -d db\`"
elif ! command -v cargo >/dev/null 2>&1; then
  skip "cargo test (Postgres integration)" "cargo is not installed"
else
  if ELASTICSEARCH_URL= ELASTICSEARCH_API_KEY= \
      cargo test --manifest-path rust/Cargo.toml; then
    pass "cargo test (Postgres integration)"
  else
    fail "cargo test (Postgres integration)"
  fi
fi

# ---------------------------------------------------------------------------
# 3. The Python suite — every tests/test_*.py, discovered. Its own
#    corpus-backed classes need the same Postgres and skip on their own with a
#    printed reason when it is absent, so this stage is not gated here.
#    tests/test_edges.mjs runs inside it (EdgeShadingTests) and skips on its
#    own if Node is missing.
# ---------------------------------------------------------------------------
header "Python suite"
if command -v python3 >/dev/null 2>&1; then
  if python3 -m unittest discover -s tests -p 'test_*.py' -v; then
    pass "python3 -m unittest (tests/test_*.py)"
  else
    fail "python3 -m unittest (tests/test_*.py)"
  fi
else
  skip "python3 -m unittest (tests/test_*.py)" "python3 is not installed"
fi

# ---------------------------------------------------------------------------
# 4. Elasticsearch-gated Rust tests — only es::integration needs both
#    Postgres and a real cluster. It indexes under an `aibrain-test-<pid>-`
#    prefix and deletes those indices itself when it finishes; nothing here
#    is a real `aibrain-*` index.
# ---------------------------------------------------------------------------
header "Rust integration tests (Elasticsearch)"
if [ "$postgres_ready" -ne 1 ]; then
  skip "cargo test es::integration (Elasticsearch)" "no Postgres reachable, see above"
elif [ "$es_ready" -ne 1 ]; then
  skip "cargo test es::integration (Elasticsearch)" \
    "ELASTICSEARCH_URL / ELASTICSEARCH_API_KEY are not set (checked the environment and a repo-root .env)"
elif ! command -v cargo >/dev/null 2>&1; then
  skip "cargo test es::integration (Elasticsearch)" "cargo is not installed"
else
  if cargo test --manifest-path rust/Cargo.toml es::integration -- --test-threads=1; then
    pass "cargo test es::integration (Elasticsearch)"
  else
    fail "cargo test es::integration (Elasticsearch)"
  fi
fi

# ---------------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------------
header "Summary"
for line in "${summary[@]}"; do
  printf '  %s\n' "$line"
done
if [ "${#skipped[@]}" -gt 0 ]; then
  echo
  echo "Skipped:"
  for line in "${skipped[@]}"; do
    printf '  - %s\n' "$line"
  done
fi

if [ "$overall_failed" -ne 0 ]; then
  echo
  echo "FAILED"
  exit 1
fi
echo
echo "All stages that ran passed."
