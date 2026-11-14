#!/usr/bin/env bash
# Regenerate and sign the apt and dnf package repository for a release.
#
#   scripts/package-repo.sh <version> [--commit]
#
# The repository is its own git repository (default ~/projects/ambersight-packages,
# $PACKAGES_REPO), served by GitHub Pages. This script takes the packages for
# <version> from dist/ and writes into it:
#   - pool/main/*.deb: the newest $KEEP_DEB versions (default 2) of each
#     architecture; older ones are removed once the new metadata is in place
#   - dists/stable/...: Packages, Release, InRelease, Release.gpg
#   - rpm/<arch>/repodata: metadata for the newest version only. The .rpm
#     files are not stored: the metadata points at the GitHub release
#     ($RPM_BASE_URL, default
#     https://github.com/HuskerMinion/AmberSight/releases/download/v<version>)
#   - setup.sh, index.html, the public key
# Running it again with the same input changes nothing. It never pushes.
# Relative paths ($PACKAGES_REPO, $APT_REPO_KEY, $DIST_DIR) are from where it
# was started.
#
# Key: $APT_REPO_KEY, default ~/.ambersight-release/apt-repo-key (secret key,
# no passphrase, readable by the owner only; never commit it). Needs gpg,
# dpkg-scanpackages, and createrepo_c (or docker, which runs it in a Fedora
# container).
set -eu
caller=$PWD
cd "$(dirname "$0")/.."
src=$PWD

usage() { echo "usage: scripts/package-repo.sh <version> [--commit]" >&2; exit 2; }
[ $# -ge 1 ] && [ $# -le 2 ] || usage
v=${1#v}
commit=0
case "${2:-}" in "") ;; --commit) commit=1 ;; *) usage ;; esac
case "$v" in *[!0-9.]* | "" | .* | *.) echo "version must look like 1.2.3, not '$v'" >&2; exit 2 ;; esac

repo=${PACKAGES_REPO:-$HOME/projects/ambersight-packages}
key=${APT_REPO_KEY:-$HOME/.ambersight-release/apt-repo-key}
dist=${DIST_DIR:-$src/dist}
# Relative paths are from where the script was started.
case "$repo" in /*) ;; *) repo=$caller/$repo ;; esac
case "$key" in /*) ;; *) key=$caller/$key ;; esac
case "$dist" in /*) ;; *) dist=$caller/$dist ;; esac
keep=${KEEP_DEB:-2}
rpmbase=${RPM_BASE_URL:-https://github.com/HuskerMinion/AmberSight/releases/download/v$v}
rpmbase=${rpmbase%/}/
[ -f "$key" ] || { echo "repository key not found: $key (see RELEASING.md)" >&2; exit 1; }
command -v dpkg-scanpackages >/dev/null || { echo "dpkg-scanpackages is needed (apt install dpkg-dev)" >&2; exit 1; }

debs=0
for a in amd64 arm64; do [ -f "$dist/ambersight_${v}_$a.deb" ] && debs=$((debs + 1)); done
[ $debs -gt 0 ] || { echo "no ambersight_${v}_*.deb in $dist" >&2; exit 1; }

mkdir -p "$repo"
[ -d "$repo/.git" ] || git -C "$repo" init -q -b main
# Metadata is made and signed in a staging folder inside the repository
# (the same file system, so the final moves are renames) and moved into place
# only when all of it is ready: a run that fails halfway leaves the old files.
rm -rf "$repo"/.stage.*
export GNUPGHOME
GNUPGHOME=$(mktemp -d)
work=$(mktemp -d)
stage=$(mktemp -d "$repo/.stage.XXXXXX")
cleanup() {
  gpgconf --homedir "$GNUPGHOME" --kill gpg-agent >/dev/null 2>&1 || true
  rm -rf "$GNUPGHOME" "$work" "$stage"
}
trap cleanup EXIT
gpg --batch --quiet --import "$key"
fpr=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr/{print $10; exit}')
sign=(gpg --batch --yes --pinentry-mode loopback --passphrase '' --digest-algo SHA512 -u "$fpr")

# ---- public files
cp scripts/package-repo/setup.sh scripts/package-repo/index.html "$repo/"
gpg --batch --export "$fpr" > "$repo/ambersight-archive-keyring.gpg"
gpg --batch --armor --export "$fpr" > "$repo/ambersight-archive-keyring.asc"
touch "$repo/.nojekyll"

# ---- deb: pool, Packages, Release
# The new .deb files go into the pool first (adding files breaks nothing).
# Packages lists the newest $keep versions of each architecture; it is made
# in the staging folder from hard links to them, signed there, and moved into
# place with the Release files. Only then are older .deb files deleted.
pool=$repo/pool/main
mkdir -p "$pool" "$stage/pool/main"
old_debs=()
for a in amd64 arm64; do
  f=$dist/ambersight_${v}_$a.deb
  if [ -f "$f" ]; then cmp -s "$f" "$pool/$(basename "$f")" || cp "$f" "$pool/"; fi
  n=0
  for ver in $(ls "$pool" | sed -n "s/^ambersight_\(.*\)_$a\.deb$/\1/p" | sort -V -r); do
    n=$((n + 1))
    if [ $n -le "$keep" ]; then
      ln "$pool/ambersight_${ver}_$a.deb" "$stage/pool/main/"
    else
      old_debs+=("$pool/ambersight_${ver}_$a.deb")
    fi
  done
done
(
  cd "$stage"
  for a in amd64 arm64; do
    d=dists/stable/main/binary-$a
    mkdir -p $d
    dpkg-scanpackages --multiversion --arch $a pool/main /dev/null > $d/Packages
    gzip -9nc $d/Packages > $d/Packages.gz
  done
)
(
  cd "$repo"
  hashes=$(
    cd "$stage/dists/stable"
    for f in main/binary-*/Packages main/binary-*/Packages.gz; do
      printf ' %s %s %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(wc -c < "$f")" "$f"
    done
  )
  rel=dists/stable/Release
  # Same files as the Release already there: keep it (and its signatures).
  if [ -f $rel ] && [ "$(sed -n '/^SHA256:/,$p' $rel)" = "$(printf 'SHA256:\n%s' "$hashes")" ] \
     && [ -f dists/stable/InRelease ] && [ -f dists/stable/Release.gpg ] \
     && cmp -s "$stage/dists/stable/main/binary-amd64/Packages" dists/stable/main/binary-amd64/Packages \
     && cmp -s "$stage/dists/stable/main/binary-arm64/Packages" dists/stable/main/binary-arm64/Packages; then
    echo "deb metadata unchanged"
  else
    st=$stage/dists/stable
    {
      echo "Origin: AmberSight"
      echo "Label: AmberSight"
      echo "Suite: stable"
      echo "Codename: stable"
      echo "Date: $(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S UTC')"
      echo "Architectures: amd64 arm64"
      echo "Components: main"
      echo "Description: AmberSight package repository"
      echo "SHA256:"
      echo "$hashes"
    } > "$st/Release"
    "${sign[@]}" --clearsign -o "$st/InRelease" "$st/Release"
    "${sign[@]}" --detach-sign --armor -o "$st/Release.gpg" "$st/Release"
    # All made and signed: into place. Release last: should a move fail, its
    # hashes no longer match and the next run signs again.
    for a in amd64 arm64; do
      mkdir -p dists/stable/main/binary-$a
      mv -f "$st/main/binary-$a/Packages" dists/stable/main/binary-$a/Packages
      mv -f "$st/main/binary-$a/Packages.gz" dists/stable/main/binary-$a/Packages.gz
    done
    mv -f "$st/InRelease" dists/stable/InRelease
    mv -f "$st/Release.gpg" dists/stable/Release.gpg
    mv -f "$st/Release" $rel
    echo "deb metadata written"
  fi
)
# No longer listed: their files go.
for f in ${old_debs[@]+"${old_debs[@]}"}; do rm -f "$f"; done

# ---- rpm: metadata only, pointing at the release's files
mkrepo() { # mkrepo <input dir> <output dir>
  if command -v createrepo_c >/dev/null 2>&1; then
    createrepo_c -q --baseurl "$rpmbase" -o "$2" "$1"
  else
    command -v docker >/dev/null || { echo "createrepo_c or docker is needed for the rpm repository" >&2; exit 1; }
    docker run --rm -v "$1:/in:ro" -v "$2:/out" fedora:latest sh -c \
      "dnf -y -q install createrepo_c >/dev/null 2>&1 && createrepo_c -q --baseurl '$rpmbase' -o /out /in && chown -R $(id -u):$(id -g) /out"
  fi
}
for a in x86_64 aarch64; do
  f=$dist/ambersight-$v-1.$a.rpm
  out=$repo/rpm/$a
  [ -f "$f" ] || { echo "no $(basename "$f"): rpm repository for $a not updated"; continue; }
  stamp="$v $(sha256sum "$f" | cut -d' ' -f1) $rpmbase"
  if [ -f "$out/.stamp" ] && [ "$(cat "$out/.stamp")" = "$stamp" ] && [ -f "$out/repodata/repomd.xml.asc" ]; then
    echo "rpm $a unchanged"; continue
  fi
  new=$stage/rpm-$a
  mkdir -p "$new" "$work/in-$a"
  cp "$f" "$work/in-$a/"
  mkrepo "$work/in-$a" "$new"
  "${sign[@]}" --detach-sign --armor -o "$new/repodata/repomd.xml.asc" "$new/repodata/repomd.xml"
  echo "$stamp" > "$new/.stamp"
  # Signed and complete: now it replaces the old metadata.
  mkdir -p "$(dirname "$out")"
  rm -rf "$out.old"
  [ ! -e "$out" ] || mv "$out" "$out.old"
  mv "$new" "$out"
  rm -rf "$out.old"
  echo "rpm $a written"
done

rm -rf "$stage"
echo "repository $repo is ready for $v (key $fpr)"
if [ $commit = 1 ]; then
  git -C "$repo" add -A
  if git -C "$repo" diff --cached --quiet; then
    echo "nothing new to commit"
  else
    git -C "$repo" -c user.name="${GIT_AUTHOR_NAME:-$(git -C "$src" config user.name)}" -c user.email="${GIT_AUTHOR_EMAIL:-$(git -C "$src" config user.email)}" commit -q -m "AmberSight $v"
    echo "committed; publish with: git -C $repo push origin main"
  fi
fi
