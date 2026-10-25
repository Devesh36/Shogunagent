#!/usr/bin/env bash
# User-local, pinned PostgreSQL for disposable CI databases. No Homebrew/sudo/service.
set -euo pipefail
pg_version=16.15
pg_sha256=c1575341fa7bd40f5274ea465b34390f4dc64cdd0770af327005caaeb9f6b7ed
pg_prefix="$HOME/.cache/shogun-ci/postgresql-$pg_version"
if [ ! -f "$pg_prefix/.ready" ]; then
  pg_build="$(mktemp -d /tmp/shogun-ci-pg-build.XXXXXX)"
  trap 'rm -rf "$pg_build"' EXIT
  curl --fail --location --retry 3 --silent --show-error \
    "https://ftp.postgresql.org/pub/source/v$pg_version/postgresql-$pg_version.tar.bz2" \
    -o "$pg_build/source.tar.bz2"
  printf '%s  %s\n' "$pg_sha256" "$pg_build/source.tar.bz2" | shasum -a 256 -c - >&2
  tar -xjf "$pg_build/source.tar.bz2" -C "$pg_build"
  (
    cd "$pg_build/postgresql-$pg_version"
    ./configure --prefix="$pg_prefix" --without-icu --without-readline --without-zlib
    make -s -j2
    make -s install
  ) >&2
  touch "$pg_prefix/.ready"
fi
"$pg_prefix/bin/postgres" --version >&2
printf '%s\n' "$pg_prefix/bin"
