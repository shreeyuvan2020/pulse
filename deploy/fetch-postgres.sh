#!/bin/sh
# Downloads the portable PostgreSQL 17 build that the zonky.io embedded-postgres
# project publishes on Maven Central, checks its SHA-256, and unpacks it to deploy/pg.
set -eu
cd "$(dirname "$0")"
VERSION=17.12.0
SHA256=7a8951dc117490f2d4161600836c8b1055306aff86e0a472aad55457651e5cbd
JAR="embedded-postgres-binaries-linux-amd64-$VERSION.jar"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "https://repo1.maven.org/maven2/io/zonky/test/postgres/embedded-postgres-binaries-linux-amd64/$VERSION/$JAR" -o "$TMP/$JAR"
echo "$SHA256  $TMP/$JAR" | sha256sum -c -
(cd "$TMP" && unzip -q "$JAR" '*.txz')
rm -rf pg && mkdir pg
tar xJf "$TMP"/postgres-linux-*.txz -C pg
pg/bin/postgres --version
