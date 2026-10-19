#!/usr/bin/env bash
# F1.5 — Validation du schéma : applique TOUTES les migrations (ordre
# lexicographique) puis le seed de dev sur un PostgreSQL jetable, et vérifie
# quelques invariants. Exécuté par la CI (job `backend`, #218).
#
# Pré-requis : Docker. Aucun effet sur une base existante (conteneur dédié).
# Usage : scripts/check-migrations.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIG_DIR="$ROOT/migrations"
CONTAINER="autome-migcheck-$$"
# Même version majeure que docker-compose.yml et k8s/postgres.yaml ;
# surchargeable (PGIMAGE=postgres:17-alpine scripts/check-migrations.sh).
PGIMAGE="${PGIMAGE:-postgres:16-alpine}"
PGPASS="check"
PGDB="autome_check"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> Démarrage PostgreSQL jetable ($PGIMAGE)"
docker run -d --name "$CONTAINER" \
  -e POSTGRES_PASSWORD="$PGPASS" -e POSTGRES_DB="$PGDB" \
  "$PGIMAGE" >/dev/null

psql_run() { docker exec -i "$CONTAINER" psql -v ON_ERROR_STOP=1 -U postgres -d "$PGDB" "$@"; }

# L'entrypoint Postgres démarre un serveur d'init temporaire PUIS le coupe
# et relance le serveur définitif : pg_isready peut matcher le temporaire.
# On attend donc qu'un vrai SELECT réussisse de façon STABLE (3 succès
# consécutifs) pour franchir la transition "shutting down".
echo -n "==> Attente readiness (SELECT 1 stable)"
ok_streak=0
for _ in $(seq 1 60); do
  if psql_run -tA -c 'SELECT 1' >/dev/null 2>&1; then
    ok_streak=$((ok_streak + 1))
    [ "$ok_streak" -ge 3 ] && { echo " ok"; break; }
  else
    ok_streak=0
  fi
  echo -n "."; sleep 1
done
if [ "$ok_streak" -lt 3 ]; then
  echo " ÉCHEC: PostgreSQL non prêt"; docker logs --tail 20 "$CONTAINER"; exit 1
fi

echo "==> Application des migrations"
for f in $(ls "$MIG_DIR"/*.sql | sort); do
  echo "    - $(basename "$f")"
  psql_run -q < "$f"
done

echo "==> Application du seed de dev"
psql_run -q < "$MIG_DIR/seeds/dev_seed.sql"

echo "==> Vérifications d'invariants"
EXPECTED_TABLES="audit_events connector_tokens credentials login_attempts revoked_tokens run_outbox run_steps runs session_versions trigger_schedules user_permissions users webhook_endpoints workflow_versions workflows"
GOT_TABLES=$(psql_run -tA -c \
  "SELECT string_agg(tablename,' ' ORDER BY tablename)
   FROM pg_tables WHERE schemaname='public';")
if [ "$GOT_TABLES" != "$EXPECTED_TABLES" ]; then
  echo "ÉCHEC: tables attendues [$EXPECTED_TABLES], obtenues [$GOT_TABLES]"; exit 1
fi

assert_count() { # <libellé> <requête> <attendu>
  local got; got=$(psql_run -tA -c "$2")
  if [ "$got" != "$3" ]; then
    echo "ÉCHEC: $1 attendu=$3 obtenu=$got"; exit 1
  fi
  echo "    OK  $1 = $got"
}
assert_count "users seedés"        "SELECT count(*) FROM users;"                1
assert_count "workflow_versions"   "SELECT count(*) FROM workflow_versions;"    1
assert_count "run_steps"           "SELECT count(*) FROM run_steps;"            2
assert_count "current_version FK"  \
  "SELECT current_version FROM workflows WHERE name='Demo : Webhook -> HTTP';" 1
assert_count "wf-demo du compte de dev" \
  "SELECT count(*) FROM workflows w JOIN workflow_versions v
     ON v.owner_id = w.owner_id AND v.workflow_id = w.id
   WHERE w.owner_id = '00000000-0000-0000-0000-000000000001';" 1
assert_count "graph jsonb lisible" \
  "SELECT graph->'nodes'->0->>'kind' FROM workflow_versions WHERE version=1;" trigger

# Anti-régression : l'unicité email doit rejeter un doublon.
if psql_run -q -c \
  "INSERT INTO users(email,password_hash) VALUES('dev@autome.local','x');" \
  >/dev/null 2>&1; then
  echo "ÉCHEC: doublon d'email accepté (contrainte UNIQUE manquante)"; exit 1
fi
echo "    OK  contrainte users_email_unique active"

# Administration (#208) : le compte seedé n'est pas suspendu.
assert_count "compte de dev actif" \
  "SELECT count(*) FROM users WHERE disabled_at IS NULL;" 1

# Vérification de l'email (#231) : le compte seedé peut se connecter.
assert_count "compte de dev vérifié" \
  "SELECT count(*) FROM users WHERE email_verified_at IS NOT NULL;" 1

echo "==> ✅ Schéma valide : migrations + seed appliqués, invariants OK"
