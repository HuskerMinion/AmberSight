#!/usr/bin/env sh
# Build the Windows installer on Linux with Docker (NSIS):
#   dist/ambersight-<version>-windows-x86_64-setup.exe
# from the folder scripts/package.ps1 makes on Windows,
# dist/ambersight-<version>-windows-x86_64/ (ambersight.exe, the DLLs,
# config.example.toml, README.md, LICENSE, THIRD-PARTY-NOTICES.txt). Copy that folder into dist/
# here first, or name it with SRC=<folder>.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="ambersight-$version-windows-x86_64"
src="${SRC:-dist/$name}"
for f in ambersight.exe config.example.toml README.md LICENSE THIRD-PARTY-NOTICES.txt; do
  if [ ! -f "$src/$f" ]; then
    echo "$src/$f is missing. Build the Windows package with scripts/package.ps1 (on Windows) and copy dist/$name here, or set SRC=<folder>." >&2
    exit 1
  fi
done
# Windows file versions have four numbers: 0.1.0 -> 0.1.0.0
vi=$(echo "$version" | sed 's/[^0-9.].*//')
while [ "$(echo "$vi" | tr -cd . | wc -c)" -lt 3 ]; do vi="$vi.0"; done
mkdir -p dist
src_abs=$(cd "$src" && pwd)
# The exact files to install and (only those) to remove again.
files="dist/$name-files.nsh"
{
  echo '!macro INSTALL_FILES'
  for f in ambersight.exe config.example.toml README.md LICENSE THIRD-PARTY-NOTICES.txt; do echo "  File \"/pkg/$f\""; done
  for f in "$src"/*.dll; do [ -e "$f" ] && echo "  File \"/pkg/$(basename "$f")\""; done
  echo '!macroend'
  echo '!macro UNINSTALL_FILES'
  # (printf, not echo: echo would read the backslashes as escapes.)
  for f in ambersight.exe config.example.toml README.md LICENSE THIRD-PARTY-NOTICES.txt; do printf '  Delete "$INSTDIR\\%s"\n' "$f"; done
  for f in "$src"/*.dll; do [ -e "$f" ] && printf '  Delete "$INSTDIR\\%s"\n' "$(basename "$f")"; done
  echo '!macroend'
} > "$files"
docker run --rm --platform linux/amd64 \
  -v "$(pwd)":/src -w /src \
  -v "$src_abs":/pkg:ro \
  -e DEBIAN_FRONTEND=noninteractive \
  debian:trixie-slim sh -eu -c "
    apt-get update -qq && apt-get install -y -qq nsis >/dev/null
    makensis -V2 -DVERSION=$version -DVIVERSION=$vi -DSRCDIR=/pkg \
      -DICON=/src/static/icons/ambersight.ico -DOUTFILE=/src/dist/$name-setup.exe -DFILES=/src/$files \
      scripts/windows/ambersight.nsi
    chown $(id -u):$(id -g) dist/$name-setup.exe
  "
echo "dist/$name-setup.exe"
