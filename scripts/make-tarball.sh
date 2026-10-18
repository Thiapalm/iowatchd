#!/usr/bin/env bash
# Builds the release tarball layout around an already built binary.
# Usage: scripts/make-tarball.sh <name> <binary>   -> <name>.tar.gz + .sha256
set -eu
cd "$(dirname "$0")/.."
name=$1
bin=$2
rm -rf "$name"
mkdir "$name"
cp "$bin" "$name/iowatchd"
cp -r iowatchd-log iowatchd-notify man completions init config.conf \
  Makefile LICENSE README.md CHANGELOG.md "$name/"
mkdir -p "$name/packaging"
cp -r packaging/logrotate "$name/packaging/"
tar czf "$name.tar.gz" "$name"
sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
