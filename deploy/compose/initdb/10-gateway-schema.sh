#!/usr/bin/env bash
# First start of the compose Postgres only (docker-entrypoint-initdb.d): the gateway's role and
# schema, the same shape apps/api/DEPLOY.md §1 sets up on a managed database — a role that owns only
# the `relay` schema and sees nothing else — then the gateway's tables, applied as that role. The
# SQL files are the repository's own (mounted read-only at /shogun-sql); each is idempotent and
# additive.
set -euo pipefail

: "${GATEWAY_DB_PASSWORD:?GATEWAY_DB_PASSWORD must be set}"

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  -v role_password="$GATEWAY_DB_PASSWORD" <<'SQL'
CREATE ROLE shogun_relay LOGIN PASSWORD :'role_password' NOINHERIT;
ALTER ROLE shogun_relay SET statement_timeout = '5s';
CREATE SCHEMA IF NOT EXISTS relay AUTHORIZATION shogun_relay;
ALTER ROLE shogun_relay SET search_path = relay;
REVOKE ALL ON SCHEMA public FROM shogun_relay;
SQL

for file in shared-store.sql agent-keys.sql agent-receipts-settled-quantity.sql agent-oauth.sql approval-webhooks.sql; do
  echo "applying $file"
  PGPASSWORD="$GATEWAY_DB_PASSWORD" psql -v ON_ERROR_STOP=1 --username shogun_relay --dbname "$POSTGRES_DB" \
    -c 'SET search_path = relay' -f "/shogun-sql/$file"
done
