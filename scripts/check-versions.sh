#!/usr/bin/env bash
# Verifies that every file carrying the release version agrees with Cargo.toml.
# Usage: scripts/check-versions.sh [vX.Y.Z]   (optional tag to compare against)
set -u
cd "$(dirname "$0")/.." || exit 1

ver=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
fail=0

check() {
  if [ "$2" != "$ver" ]; then
    echo "MISMATCH: $1 has '$2', Cargo.toml has '$ver'" >&2
    fail=1
  else
    echo "ok: $1 = $ver"
  fi
}

if [ $# -ge 1 ]; then
  check "tag $1" "${1#v}"
fi

check Cargo.lock "$(awk '/^name = "iowatchd"/ {getline; gsub(/version = |"/, ""); print; exit}' Cargo.lock)"
for m in man/*; do
  check "$m" "$(sed -n '1s/^\.TH .* "iowatchd \([^"]*\)".*/\1/p' "$m")"
done
check packaging/arch/PKGBUILD "$(sed -n 's/^pkgver=//p' packaging/arch/PKGBUILD)"
check packaging/rpm/iowatchd.spec "$(awk '/^Version:/ {print $2}' packaging/rpm/iowatchd.spec)"
check "packaging/rpm changelog" "$(sed -n '/^%changelog/{n;s/.* - \([0-9.]*\)-[0-9]*$/\1/p}' packaging/rpm/iowatchd.spec)"
check packaging/debian/changelog "$(sed -n '1s/^iowatchd (\([0-9.]*\)-.*/\1/p' packaging/debian/changelog)"

top=$(sed -n 's/^## \[\([0-9][^]]*\)\].*/\1/p' CHANGELOG.md | head -1)
check CHANGELOG.md "$top"

# The binary recipes are bumped after the release, so only require that they
# are internally consistent and not newer than the source version.
check_bin() {
  local file=$1 declared=$2 urls=$3
  if [ -z "$declared" ] || [ "$(printf '%s\n%s\n' "$declared" "$ver" | sort -V | tail -1)" != "$ver" ]; then
    echo "MISMATCH: $file declares '$declared', newer than Cargo.toml '$ver'" >&2
    fail=1
  fi
  if [ -n "$(printf '%s\n' "$urls" | grep -v "v$declared" )" ]; then
    echo "MISMATCH: $file has URLs that do not match version $declared" >&2
    fail=1
  fi
  echo "ok: $file = $declared"
}
check_bin packaging/arch-bin/PKGBUILD "$(sed -n 's/^pkgver=//p' packaging/arch-bin/PKGBUILD)" ""
check_bin packaging/homebrew/iowatchd.rb "$(sed -n 's/^ *url ".*download\/v\([^/]*\)\/.*/\1/p' packaging/homebrew/iowatchd.rb | sort -u | head -1)" \
  "$(sed -n 's/^ *url ".*download\/\(v[^/]*\)\/.*/\1/p' packaging/homebrew/iowatchd.rb)"

exit $fail
