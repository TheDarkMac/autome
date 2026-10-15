#!/usr/bin/env bash
# Smoke test de bout en bout de la stack Docker Compose (#178).
#
# Prouve que l'API et le worker partagent réellement PostgreSQL et RabbitMQ :
# un run déclenché via l'API doit être exécuté par le worker et passer
# `succeeded`. (Avec des backends en mémoire, il resterait `queued`.)
#
# Prérequis : la stack tourne (`docker compose up --build -d`), `curl` et
# `python3` disponibles. Usage :
#
#   scripts/compose-smoke.sh                      # API sur http://localhost:8080
#   AUTOME_API=http://host:8080 scripts/compose-smoke.sh
#
# Crée un compte jetable (email unique) et un workflow trigger-only — aucun
# effet de bord externe.
set -euo pipefail

API="${AUTOME_API:-http://localhost:8080}"
TIMEOUT_S="${SMOKE_TIMEOUT_S:-60}"
SUFFIX="$(date +%s)-$$"
EMAIL="smoke-${SUFFIX}@autome.local"
PASSWORD="smoke-password-${SUFFIX}"
WORKFLOW_ID="smoke-${SUFFIX}"

fail() { echo "ÉCHEC : $*" >&2; exit 1; }
json_field() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }

# `/readyz` (#192) : prête = migrations appliquées, base et broker joignables.
echo "→ attente de l'API ($API/readyz)…"
for _ in $(seq 1 "$TIMEOUT_S"); do
  curl -fsS "$API/readyz" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "$API/readyz" >/dev/null || fail "API pas prête sur $API ($(curl -s "$API/readyz"))"

echo "→ inscription + connexion ($EMAIL)"
curl -fsS -H 'content-type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
  "$API/auth/signup" >/dev/null || fail "signup refusé"
TOKEN="$(curl -fsS -H 'content-type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
  "$API/auth/login" | json_field access_token)" || fail "login refusé"
AUTH=(-H "authorization: Bearer $TOKEN")

echo "→ création du workflow $WORKFLOW_ID"
curl -fsS "${AUTH[@]}" -H 'content-type: application/json' -d "{
  \"id\": \"$WORKFLOW_ID\",
  \"name\": \"Smoke compose\",
  \"nodes\": [{\"id\": \"entry\", \"name\": \"entry\", \"kind\": \"trigger\",
               \"trigger_type\": \"manual\", \"params\": null}],
  \"edges\": []
}" "$API/workflows" >/dev/null || fail "création du workflow refusée"

echo "→ déclenchement d'un run"
RUN_ID="$(curl -fsS "${AUTH[@]}" -H 'content-type: application/json' -d '{}' \
  "$API/workflows/$WORKFLOW_ID/runs" | json_field run_id)" || fail "run refusé"
echo "  run_id = $RUN_ID"

echo "→ attente de l'exécution par le worker (≤ ${TIMEOUT_S} s)…"
STATUS=""
for _ in $(seq 1 "$TIMEOUT_S"); do
  STATUS="$(curl -fsS "${AUTH[@]}" "$API/runs/$RUN_ID" | json_field status | tr '[:upper:]' '[:lower:]')"
  case "$STATUS" in
    succeeded) echo "OK : run $RUN_ID exécuté par le worker (succeeded)"; exit 0 ;;
    failed) fail "run $RUN_ID en échec : $(curl -fsS "${AUTH[@]}" "$API/runs/$RUN_ID")" ;;
  esac
  sleep 1
done
fail "run $RUN_ID toujours « $STATUS » après ${TIMEOUT_S} s — le worker ne consomme pas la file de l'API ?"
