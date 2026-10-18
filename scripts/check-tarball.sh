#!/usr/bin/env bash
# Smoke-tests a release tarball: static binary, expected files, staged install.
# Usage: [REQUIRE_STATIC=1] scripts/check-tarball.sh <tarball>
set -eu
tarball=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar xzf "$tarball" -C "$work"
cd "$work"/*/

for f in iowatchd iowatchd-log iowatchd-notify man/iowatchd.8 man/iowatchd.conf.5 \
  man/iowatchd-log.1 completions/iowatchd.bash completions/_iowatchd \
  completions/iowatchd.fish init/systemd/iowatchd.service config.conf Makefile \
  LICENSE packaging/logrotate/iowatchd; do
  test -e "$f" || { echo "missing in tarball: $f" >&2; exit 1; }
done

if [ "${REQUIRE_STATIC:-0}" = 1 ] && ldd iowatchd 2>&1 | grep -qv "not a dynamic\|statically linked"; then
  echo "iowatchd is not statically linked" >&2
  ldd iowatchd >&2
  exit 1
fi

make install BIN=./iowatchd DESTDIR="$work/stage" PREFIX=/usr
test -x "$work/stage/usr/bin/iowatchd"
test -f "$work/stage/etc/iowatchd.conf"
"$work/stage/usr/bin/iowatchd" --version
