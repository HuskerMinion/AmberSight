#!/usr/bin/env sh
# Gather the language packs for a release:
#   dist/ambersight-lang-<code>-<version>.json   for every lang/<code>.json (not en.json, the source, nor en-XA, the test pseudo-language)
# and print their lines for SHA256SUMS ("<sha256>  <file>").
#
# Each file is byte for byte the repo's lang/<code>.json: build.rs fixed that file's hash into the
# program, and AmberSight accepts a pack only if it has that hash (src/lang.rs). So run this from
# the same tree the release was built from; a pack changed after the build is refused by every
# copy of that build. Packs of a release are listed in its signed SHA256SUMS too.
#
# scripts/release.sh requires every pack and checks each is the same bytes as lang/<code>.json.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
mkdir -p dist
n=0
for f in lang/*.json; do
  [ -f "$f" ] || continue
  code=$(basename "$f" .json)
  [ "$code" = en ] && continue
  # en-XA is the pseudo-language for testing layouts: a developer tool, never published
  [ "$code" = en-XA ] && continue
  case "$code" in
    *[!A-Za-z0-9-]*|'') echo "skipping $f: not a language code" >&2; continue ;;
  esac
  out="dist/ambersight-lang-$code-$version.json"
  cp "$f" "$out"
  chmod 644 "$out"
  n=$((n + 1))
done
[ "$n" -gt 0 ] || { echo "no packs in lang/ (only en.json?)" >&2; exit 1; }
cd dist
sha256sum "ambersight-lang-"*"-$version.json"
