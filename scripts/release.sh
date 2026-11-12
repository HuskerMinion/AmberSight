#!/usr/bin/env sh
# Prepare (and, with --publish, publish) a signed AmberSight release.
#
#   scripts/release.sh <version>            prepare only (the default)
#   scripts/release.sh <version> --publish  also create the GitHub release
#
# Before running it, build the packages for each computer with the existing
# scripts (scripts/package.sh, scripts/package.ps1 on Windows,
# scripts/package-linux-arm64.sh, ...) and gather them in dist/. Whatever
# exists of these is included:
#   ambersight-<v>-windows-x86_64.zip         ambersight-<v>-windows-x86_64-setup.exe
#   ambersight-<v>-linux-x86_64.tar.gz        ambersight-<v>-linux-aarch64.tar.gz
#   ambersight-<v>-darwin-arm64.tar.gz        ambersight-<v>-darwin-x86_64.tar.gz
#   ambersight_<v>_amd64.deb                  ambersight_<v>_arm64.deb
#   ambersight-<v>-1.x86_64.rpm               ambersight-<v>-1.aarch64.rpm
#
# It writes dist/release-<v>/SHA256SUMS over them and signs it with the
# offline release key (dist/release-<v>/SHA256SUMS.sig), using this
# machine's built program (`ambersight release-sign`). The updater in
# AmberSight refuses any release whose SHA256SUMS.sig does not match the
# public key built into it.
#
# Key: $RELEASE_KEY, default ~/.ambersight-release/release-key.pk8 (see
# RELEASING.md; never commit it). Program: $AMBERSIGHT_BIN, default
# target/release/ambersight (built if missing).
#
# --publish runs `gh release create v<v>` in the public repository
# ($RELEASE_REPO, default HuskerMinion/AmberSight: the one the updater reads)
# with every package plus SHA256SUMS and SHA256SUMS.sig, for the tag v<v>
# already pushed there. It refuses a private repository (the updater could
# not see the release).
set -eu
cd "$(dirname "$0")/.."

usage() { echo "usage: scripts/release.sh <version> [--publish]" >&2; exit 2; }
[ $# -ge 1 ] || usage
v=${1#v}
publish=0
[ $# -le 2 ] || usage
case "${2:-}" in
  "") ;;
  --publish) publish=1 ;;
  *) usage ;;
esac
case "$v" in
  *[!0-9.]* | "" | .* | *.) echo "version must look like 1.2.3, not '$v'" >&2; exit 2 ;;
esac

own=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [ "$own" != "$v" ]; then
  echo "warning: Cargo.toml says version $own, not $v (the updater checks the program reports $v)" >&2
fi

key=${RELEASE_KEY:-$HOME/.ambersight-release/release-key.pk8}
[ -f "$key" ] || { echo "release key not found: $key (see RELEASING.md)" >&2; exit 1; }

bin=${AMBERSIGHT_BIN:-target/release/ambersight}
if [ ! -x "$bin" ]; then
  cargo build --release
  bin=target/release/ambersight
fi

files=""
for f in \
  "ambersight-$v-windows-x86_64.zip" "ambersight-$v-windows-x86_64-setup.exe" \
  "ambersight-$v-linux-x86_64.tar.gz" "ambersight-$v-linux-aarch64.tar.gz" \
  "ambersight-$v-darwin-arm64.tar.gz" "ambersight-$v-darwin-x86_64.tar.gz" \
  "ambersight_${v}_amd64.deb" "ambersight_${v}_arm64.deb" \
  "ambersight-$v-1.x86_64.rpm" "ambersight-$v-1.aarch64.rpm"; do
  if [ -f "dist/$f" ]; then
    files="$files $f"
  fi
done
[ -n "$files" ] || { echo "no packages for $v in dist/" >&2; exit 1; }

out="dist/release-$v"
mkdir -p "$out"
if command -v sha256sum >/dev/null 2>&1; then
  sum="sha256sum"
else
  sum="shasum -a 256" # macOS
fi
# Names only (no folders), as the updater looks them up.
# shellcheck disable=SC2086
(cd dist && $sum $files) > "$out/SHA256SUMS"
# Fails when the key is not the one built into the program.
"$bin" release-sign --key "$key" "$out/SHA256SUMS"
echo "release $v prepared in $out:"
cat "$out/SHA256SUMS"

if [ $publish = 1 ]; then
  command -v gh >/dev/null 2>&1 || { echo "gh (GitHub CLI) is needed to publish" >&2; exit 1; }
  repo=${RELEASE_REPO:-HuskerMinion/AmberSight}
  private=$(gh repo view "$repo" --json isPrivate --jq .isPrivate) || { echo "cannot see $repo" >&2; exit 1; }
  [ "$private" = false ] || { echo "$repo is private: the updater could not see this release" >&2; exit 1; }
  assets=""
  for f in $files; do assets="$assets dist/$f"; done
  # shellcheck disable=SC2086
  gh release create "v$v" --repo "$repo" $assets "$out/SHA256SUMS" "$out/SHA256SUMS.sig" \
    --title "AmberSight $v" --notes "AmberSight $v" --verify-tag
  echo "published v$v in $repo"
else
  echo "not published (add --publish to publish it)"
fi
