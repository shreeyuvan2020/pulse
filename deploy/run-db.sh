#!/bin/sh
# Pulse's own Postgres. It listens on a Unix socket only, inside a folder only
# this deployment's account can open, so nothing else on the Box can reach it.
set -eu
DATA="$AMP_DEPLOYMENT_DATA_DIRECTORY/pg"
SOCK="$AMP_DEPLOYMENT_DATA_DIRECTORY/pgsock"
PG="$PWD/deploy/pg/bin"
mkdir -p "$SOCK" && chmod 700 "$SOCK"
if [ ! -f "$DATA/PG_VERSION" ]; then
  "$PG/initdb" -D "$DATA" -U pulse -A trust -E UTF8 --locale=C
fi
exec "$PG/postgres" -D "$DATA" \
  -c listen_addresses= \
  -c unix_socket_directories="$SOCK" \
  -c unix_socket_permissions=0700 \
  -c max_connections=60 \
  -c shared_buffers=256MB
