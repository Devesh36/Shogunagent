#!/usr/bin/env bash
# Disposable PostgreSQL 16 for the website E2E steps (website.yml), the same on GitHub's
# ubuntu-latest and on the Mac mini's self-hosted Linux runner (docs/mac-mini-linux-runner.md).
#
#   scripts/ci-postgres.sh start   new cluster in $RUNNER_TEMP on 127.0.0.1 and a free port,
#                                  database shogun_waitlist, DATABASE_URL exported via $GITHUB_ENV
#   scripts/ci-postgres.sh stop    stop it and delete its data (run it with `if: always()`)
#
# It replaces the postgres:16 service container: service containers need Docker, which the
# self-hosted VM does not run, and a fixed host port would collide when two runner instances run
# the website job at once. Every job gets an empty database, as the container gave it. Never
# point this at a real database: it only ever creates its own.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# Write NAME=VALUE for the following steps (GitHub Actions) or print it (by hand).
export_var() {
  if [ -n "${GITHUB_ENV:-}" ]; then
    printf '%s=%s\n' "$1" "$2" >> "$GITHUB_ENV"
  else
    printf 'export %s=%q\n' "$1" "$2"
  fi
}

pg_bin() {
  # Ubuntu's postgresql-16 package: preinstalled on GitHub's image, installed in the VM by
  # scripts/mac-mini-linux-runner/provision.sh. Otherwise the pinned source build, cached.
  if [ -x /usr/lib/postgresql/16/bin/initdb ]; then
    echo /usr/lib/postgresql/16/bin
  else
    "$here/ci-postgres-bin.sh"
  fi
}

start() {
  local bin root port
  bin=$(pg_bin)
  root=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/shogun-pg.XXXXXX")
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  "$bin/initdb" -D "$root/data" -U postgres --auth=trust --no-locale --encoding=UTF8 >/dev/null
  # Loopback TCP plus a socket inside the cluster's own directory; nothing else can reach it.
  "$bin/pg_ctl" -D "$root/data" -l "$root/server.log" -w \
    -o "-h 127.0.0.1 -p $port -k $root" start >/dev/null
  "$bin/createdb" -h 127.0.0.1 -p "$port" -U postgres shogun_waitlist
  "$bin/postgres" --version
  echo "disposable Postgres on 127.0.0.1:$port"
  export_var SHOGUN_CI_PG_ROOT "$root"
  export_var SHOGUN_CI_PG_BIN "$bin"
  # Trust auth: the password is ignored; it is kept so the URL has the shape the app expects.
  export_var DATABASE_URL "postgres://postgres:postgres@127.0.0.1:$port/shogun_waitlist"
}

stop() {
  local root=${SHOGUN_CI_PG_ROOT:-} bin=${SHOGUN_CI_PG_BIN:-}
  if [ -z "$root" ]; then
    echo "no disposable Postgres was started"
    return
  fi
  case "$root" in
    */shogun-pg.*) ;;
    *) echo "refusing to remove unexpected path $root" >&2; exit 1 ;;
  esac
  if [ -n "$bin" ] && [ -d "$root/data" ]; then
    "$bin/pg_ctl" -D "$root/data" -m immediate stop >/dev/null 2>&1 || true
  fi
  rm -rf -- "$root"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
