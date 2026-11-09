#!/usr/bin/env bash
# Two quick curl checks for an A2A agent configured in the app: the A2A agent
# card, and the Elasticsearch root of the same deployment.
#
#   scripts/check-elastic.sh                  # first a2a agent in the config
#   scripts/check-elastic.sh <agent-id>       # a specific agent
#   scripts/check-elastic.sh <agent-id> --send  # also post a message/send ping
#
# Everything comes from the app's config ($AIBRAIN_HOME/config.json, default
# ~/.aibrain/config.json): the agent's `url` and its `headers` (the API key).
# Agent Builder is served by Kibana, so the Elasticsearch URL is the agent's
# host with `.kb.` swapped for `.es.`.

set -uo pipefail

CONFIG="${AIBRAIN_HOME:-$HOME/.aibrain}/config.json"
[ -f "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 1; }

AGENT_ID="${1:-}"
SEND="${2:-}"

# Prints: agent id, url, then one `Name: value` line per configured header.
# The key stays in this process and the curl arguments; it is never echoed.
AGENT=$(python3 -I - "$CONFIG" "$AGENT_ID" <<'PY'
import json, sys
config, want = sys.argv[1], sys.argv[2]
agents = [a for a in json.load(open(config)).get("agents", []) if a.get("kind") == "a2a"]
if want:
    agents = [a for a in agents if a.get("id") == want]
if not agents:
    sys.exit("no a2a agent %r in %s" % (want or "(any)", config))
agent = agents[0]
print(agent["id"])
print(agent.get("url", ""))
for name, value in (agent.get("headers") or {}).items():
    print("%s: %s" % (name, value))
PY
) || exit 1

ID=$(printf '%s\n' "$AGENT" | sed -n 1p)
CARD_URL=$(printf '%s\n' "$AGENT" | sed -n 2p)
HEADERS=()
while IFS= read -r line; do
  [ -n "$line" ] && HEADERS+=(-H "$line")
done < <(printf '%s\n' "$AGENT" | sed -n '3,$p')

[ -n "$CARD_URL" ] || { echo "agent $ID has no url" >&2; exit 1; }

# Scheme + host of the agent URL, then .kb. -> .es.
ORIGIN=$(printf '%s' "$CARD_URL" | sed -E 's#^(https?://[^/]+).*#\1#')
ES_URL="${ORIGIN/.kb./.es.}"

# curl prints the body, then a final line with the HTTP status.
call() {
  curl -sS -m 30 -w '\nHTTP %{http_code}\n' "$@"
}

echo "==> agent: $ID"
echo
echo "==> Elasticsearch root: $ES_URL"
call "${HEADERS[@]}" "$ES_URL/"

echo
echo "==> A2A agent card: $CARD_URL"
call "${HEADERS[@]}" -H 'Accept: application/json' "$CARD_URL"

[ "$SEND" = "--send" ] || exit 0

# The card's URL is the JSON-RPC endpoint without the `.json` suffix.
echo
echo "==> A2A message/send: ${CARD_URL%.json}"
call -X POST "${CARD_URL%.json}" \
  "${HEADERS[@]}" -H 'Content-Type: application/json' -H 'Accept: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "check-1",
    "method": "message/send",
    "params": {"message": {
      "role": "user",
      "kind": "message",
      "messageId": "check-msg-1",
      "parts": [{"kind": "text", "text": "Say hello in one sentence."}]
    }}
  }'
