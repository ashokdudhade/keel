#!/bin/sh
# Install Keel from GitHub Releases (SHA-256 verified).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
#
# Environment:
#   KEEL_VERSION      Release tag or "latest" (default: latest)
#   KEEL_INSTALL_DIR  Install directory (default: $HOME/.local/bin)
#   KEEL_REPO         owner/repo (default: ashokdudhade/keel)

set -eu

REPO="${KEEL_REPO:-ashokdudhade/keel}"
VERSION="${KEEL_VERSION:-latest}"
INSTALL_DIR="${KEEL_INSTALL_DIR:-$HOME/.local/bin}"

err() {
  printf 'keel installer: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

need_cmd curl
need_cmd tar
need_cmd mktemp
need_cmd uname

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS_KEY="apple-darwin" ;;
  Linux) OS_KEY="unknown-linux-gnu" ;;
  *) err "unsupported OS: $OS (supported: Darwin, Linux)" ;;
esac

case "$ARCH" in
  x86_64|amd64) ARCH_KEY="x86_64" ;;
  arm64|aarch64) ARCH_KEY="aarch64" ;;
  *) err "unsupported architecture: $ARCH (supported: x86_64, arm64)" ;;
esac

TARGET="${ARCH_KEY}-${OS_KEY}"

if [ "$VERSION" = "latest" ]; then
  API_URL="https://api.github.com/repos/${REPO}/releases/latest"
  TAG="$(curl -fsSL "$API_URL" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$TAG" ] || err "could not resolve latest release tag from $API_URL"
  VERSION="$TAG"
fi

# Strip leading v from version for archive name when present in both forms.
VERSION_NUM="${VERSION#v}"
ARCHIVE="keel-${VERSION_NUM}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
ARCHIVE_URL="${BASE_URL}/${ARCHIVE}"
SUMS_URL="${BASE_URL}/SHA256SUMS"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT HUP TERM

printf 'Downloading %s\n' "$ARCHIVE_URL"
curl -fsSL "$ARCHIVE_URL" -o "$TMP/$ARCHIVE" || err "failed to download $ARCHIVE_URL"
curl -fsSL "$SUMS_URL" -o "$TMP/SHA256SUMS" || err "failed to download $SUMS_URL"

EXPECTED="$(
  awk -v f="$ARCHIVE" '
    $2 == f || $2 == ("*" f) { print $1; exit }
  ' "$TMP/SHA256SUMS"
)"
[ -n "$EXPECTED" ] || err "checksum for $ARCHIVE not found in SHA256SUMS"

if command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$TMP/$ARCHIVE" | awk '{print $1}')"
else
  err "need shasum or sha256sum to verify downloads"
fi

[ "$ACTUAL" = "$EXPECTED" ] || err "SHA-256 mismatch for $ARCHIVE (expected $EXPECTED, got $ACTUAL)"

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
[ -f "$TMP/keel" ] || err "archive did not contain keel binary"

mkdir -p "$INSTALL_DIR"
cp "$TMP/keel" "$INSTALL_DIR/keel"
chmod 755 "$INSTALL_DIR/keel"

printf 'Installed keel %s to %s/keel\n' "$VERSION" "$INSTALL_DIR"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf '\nAdd this to your shell profile so keel is on PATH:\n'
    printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    ;;
esac

"$INSTALL_DIR/keel" --help >/dev/null 2>&1 || true
printf 'Done.\n'
printf '\nNext steps:\n'
printf '  brew services start keel   # global daemon (Homebrew)\n'
printf '  # or: keel daemon\n'
printf '  cd /path/to/your/project\n'
printf '  keel start                 # index + watch this project\n'
printf '  keel definition SomeSymbol # auto-indexes if needed\n'
printf '  keel stop                  # stop watching this project\n'
