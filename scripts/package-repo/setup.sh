#!/bin/sh
# Adds the AmberSight package repository to this computer: installs the
# signing key and the apt source (Debian, Ubuntu, Raspberry Pi OS) or the dnf
# repository (Fedora). It changes nothing else. Run it as root:
#
#   curl -fsSL https://huskerminion.github.io/ambersight-packages/setup.sh | sudo sh
#
# Afterwards: sudo apt update && sudo apt install ambersight
#        or:  sudo dnf install ambersight
set -eu

# Everything is in main, called on the last line: a download cut short runs
# nothing.
main() {
  BASE=${AMBERSIGHT_REPO_URL:-https://huskerminion.github.io/ambersight-packages}
  BASE=${BASE%/}

  die() { echo "ambersight setup: $*" >&2; exit 1; }
  [ "$(id -u)" = 0 ] || die "run as root (use sudo)"

  case "$BASE" in
    https://*) ;;
    *) die "the repository address must start with https:// (not $BASE)" ;;
  esac

  fetch() { # fetch <url> <file>: HTTPS only (curl: TLS 1.2 or newer)
    if command -v curl >/dev/null 2>&1; then
      curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
      # No redirects (the repository serves these files directly), so an
      # https:// address stays https://. A custom domain for the Pages site
      # would redirect here: then point BASE at it.
      wget -q --max-redirect=0 -O "$2" "$1"
    else
      die "curl or wget is needed"
    fi
  }

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  if command -v apt-get >/dev/null 2>&1 && command -v dpkg >/dev/null 2>&1; then
    arch=$(dpkg --print-architecture)
    case "$arch" in
      amd64 | arm64) ;;
      *) die "no AmberSight packages for $arch (64-bit Intel/AMD and ARM only)" ;;
    esac
    fetch "$BASE/ambersight-archive-keyring.gpg" "$tmp/key.gpg"
    [ -s "$tmp/key.gpg" ] || die "the key download is empty"
    install -d -m 0755 /etc/apt/keyrings
    install -m 0644 "$tmp/key.gpg" /etc/apt/keyrings/ambersight.gpg
    cat > /etc/apt/sources.list.d/ambersight.sources <<EOF
Types: deb
URIs: $BASE
Suites: stable
Components: main
Architectures: $arch
Signed-By: /etc/apt/keyrings/ambersight.gpg
EOF
    echo "Added /etc/apt/sources.list.d/ambersight.sources ($arch)."
    echo "Next: sudo apt update && sudo apt install ambersight"
  elif command -v dnf >/dev/null 2>&1; then
    case "$(uname -m)" in
      x86_64 | aarch64) ;;
      *) die "no AmberSight packages for $(uname -m) (64-bit Intel/AMD and ARM only)" ;;
    esac
    # repo_gpgcheck: the repository's metadata is signed; it lists each
    # package's checksum. The rpm files themselves are not signed.
    cat > /etc/yum.repos.d/ambersight.repo <<EOF
[ambersight]
name=AmberSight
baseurl=$BASE/rpm/\$basearch
enabled=1
gpgcheck=0
repo_gpgcheck=1
gpgkey=$BASE/ambersight-archive-keyring.asc
EOF
    echo "Added /etc/yum.repos.d/ambersight.repo."
    echo "Next: sudo dnf install ambersight   (Fedora also needs RPM Fusion for the full ffmpeg; see the install guide)"
  else
    die "this needs apt (Debian, Ubuntu, Raspberry Pi OS) or dnf (Fedora)"
  fi
}

main "$@"
