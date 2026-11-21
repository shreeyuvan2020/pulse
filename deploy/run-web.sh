#!/bin/sh
# The Pulse server. Waits for the database service's socket, then serves on the
# port the Box routes to. (The portable Postgres has no client tools, so the
# server uses the `postgres` database that initdb creates, and if it races the
# database coming up it simply exits and the Box restarts it.)
set -eu
SOCK="$AMP_DEPLOYMENT_DATA_DIRECTORY/pgsock"
until [ -S "$SOCK/.s.PGSQL.5432" ]; do sleep 1; done
sleep 2
export DATABASE_URL="postgres://pulse@localhost/postgres?host=$SOCK"
export PULSE_BIND="0.0.0.0:$PORT"
export PULSE_BASE_URL="$AMP_DEPLOYMENT_URL"
export PULSE_STORAGE_DIR="$AMP_DEPLOYMENT_DATA_DIRECTORY/recordings"
exec "$PWD/deploy/bin/pulse-server"
