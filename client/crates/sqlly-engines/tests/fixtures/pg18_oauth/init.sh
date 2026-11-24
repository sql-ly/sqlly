#!/bin/bash
# Runs once at first start (docker-entrypoint-initdb.d): roles + pg_hba.
set -euo pipefail

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" <<'SQL'
CREATE ROLE alice LOGIN;
CREATE ROLE bob LOGIN PASSWORD 'bob-password';
SQL

cat > "$PGDATA/pg_hba.conf" <<'HBA'
local   all  all                  trust
hostssl all  alice  all  oauth issuer="https://idp.example.com" scope="openid postgres" delegate_ident_mapping=1
host    all  all    all  scram-sha-256
HBA
