#!/bin/sh
# Install Keel from GitHub Releases (SHA-256 verified).
#
# Detects OS/arch, installs the matching binary, and adds the install dir to
# PATH in common shell profiles so `keel` works in new terminals.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
#
# Environment:
#   KEEL_VERSION         Release tag or "latest" (default: latest)
#   KEEL_INSTALL_DIR     Install directory (default: $HOME/.local/bin)
#   KEEL_REPO            owner/repo (default: ashokdudhade/keel)
#   KEEL_NO_MODIFY_PATH  Set to 1 to skip editing shell profiles

set -eu

REPO="${KEEL_REPO:-ashokdudhade/keel}"
VERSION="${KEEL_VERSION:-latest}"
INSTALL_DIR="${KEEL_INSTALL_DIR:-$HOME/.local/bin}"
NO_MODIFY_PATH="${KEEL_NO_MODIFY_PATH:-0}"

MARKER_BEGIN="# >>> keel PATH (managed by install.sh) >>>"
MARKER_END="# <<< keel PATH (managed by install.sh) <<<"

err() {
  printf 'keel installer: %s\n' "$*" >&2
  exit 1
}

info() {
  printf 'keel installer: %s\n' "$*"
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

need_cmd curl
need_cmd tar
need_cmd mktemp
need_cmd uname

# --- OS / arch detection -----------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    OS_NAME="macOS"
    OS_KEY="apple-darwin"
    ;;
  Linux)
    OS_NAME="Linux"
    OS_KEY="unknown-linux-gnu"
    ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    err "native Windows is not supported yet. Use WSL2 and run this installer inside Linux."
    ;;
  *)
    err "unsupported OS: $OS (supported: macOS, Linux; Windows via WSL2)"
    ;;
esac

case "$ARCH" in
  x86_64|amd64)
    ARCH_NAME="x86_64"
    ARCH_KEY="x86_64"
    ;;
  arm64|aarch64)
    ARCH_NAME="arm64"
    ARCH_KEY="aarch64"
    ;;
  *)
    err "unsupported architecture: $ARCH (supported: x86_64, arm64)"
    ;;
esac

TARGET="${ARCH_KEY}-${OS_KEY}"
info "detected ${OS_NAME} ${ARCH_NAME} → ${TARGET}"

# --- Resolve release + download ----------------------------------------------

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

info "downloading ${ARCHIVE_URL}"
curl -fsSL "$ARCHIVE_URL" -o "$TMP/$ARCHIVE" || err "failed to download $ARCHIVE_URL (no asset for ${TARGET}?)"
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
info "installed keel ${VERSION} → ${INSTALL_DIR}/keel"

# --- PATH: current process + shell profiles ----------------------------------

path_contains_install_dir() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
    *) return 1 ;;
  esac
}

# Available to anything that inherits this process environment.
export PATH="${INSTALL_DIR}:${PATH}"

ensure_posix_path_line() {
  printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
}

write_posix_profile() {
  _file="$1"
  _dir="$(dirname "$_file")"
  mkdir -p "$_dir"
  if [ -f "$_file" ] && grep -F "$MARKER_BEGIN" "$_file" >/dev/null 2>&1; then
    _tmp="${_file}.keel-tmp"
    awk -v begin="$MARKER_BEGIN" -v end="$MARKER_END" -v dir="$INSTALL_DIR" '
      $0 == begin { skip=1; print begin; printf "export PATH=\"%s:$PATH\"\n", dir; next }
      $0 == end { skip=0; print end; next }
      skip { next }
      { print }
    ' "$_file" > "$_tmp"
    if ! grep -F "$MARKER_BEGIN" "$_tmp" >/dev/null 2>&1; then
      {
        printf '\n%s\n' "$MARKER_BEGIN"
        ensure_posix_path_line
        printf '%s\n' "$MARKER_END"
      } >> "$_tmp"
    fi
    mv "$_tmp" "$_file"
    info "updated PATH in ${_file}"
    return
  fi
  {
    printf '\n%s\n' "$MARKER_BEGIN"
    ensure_posix_path_line
    printf '%s\n' "$MARKER_END"
  } >> "$_file"
  info "added PATH to ${_file}"
}

write_fish_path() {
  _file="$1"
  mkdir -p "$(dirname "$_file")"
  if [ -f "$_file" ] && grep -F "$MARKER_BEGIN" "$_file" >/dev/null 2>&1; then
    _tmp="${_file}.keel-tmp"
    awk -v begin="$MARKER_BEGIN" -v end="$MARKER_END" -v dir="$INSTALL_DIR" '
      $0 == begin { skip=1; print begin; printf "fish_add_path -g %s\n", dir; next }
      $0 == end { skip=0; print end; next }
      skip { next }
      { print }
    ' "$_file" > "$_tmp"
    if ! grep -F "$MARKER_BEGIN" "$_tmp" >/dev/null 2>&1; then
      {
        printf '\n%s\n' "$MARKER_BEGIN"
        printf 'fish_add_path -g %s\n' "$INSTALL_DIR"
        printf '%s\n' "$MARKER_END"
      } >> "$_tmp"
    fi
    mv "$_tmp" "$_file"
    info "updated PATH in ${_file}"
    return
  fi
  {
    printf '\n%s\n' "$MARKER_BEGIN"
    printf 'fish_add_path -g %s\n' "$INSTALL_DIR"
    printf '%s\n' "$MARKER_END"
  } >> "$_file"
  info "added PATH to ${_file}"
}

if [ "$NO_MODIFY_PATH" = "1" ]; then
  info "KEEL_NO_MODIFY_PATH=1 — skipped shell profile edits"
  if ! path_contains_install_dir; then
    info "add to PATH manually: export PATH=\"${INSTALL_DIR}:\$PATH\""
  fi
else
  # Login shells / many GUI apps read ~/.profile.
  write_posix_profile "${HOME}/.profile"

  SHELL_NAME="$(basename "${SHELL:-}")"
  case "$SHELL_NAME" in
    zsh)
      write_posix_profile "${HOME}/.zshrc"
      ;;
    bash)
      write_posix_profile "${HOME}/.bashrc"
      # macOS Terminal often starts login bash → .bash_profile only.
      if [ "$OS_NAME" = "macOS" ]; then
        write_posix_profile "${HOME}/.bash_profile"
      fi
      ;;
    fish)
      write_fish_path "${HOME}/.config/fish/config.fish"
      ;;
    *)
      [ -f "${HOME}/.zshrc" ] && write_posix_profile "${HOME}/.zshrc"
      [ -f "${HOME}/.bashrc" ] && write_posix_profile "${HOME}/.bashrc"
      ;;
  esac
fi

# --- Verify ------------------------------------------------------------------

if ! "$INSTALL_DIR/keel" --help >/dev/null 2>&1; then
  err "installed binary at ${INSTALL_DIR}/keel failed to run"
fi

if command -v keel >/dev/null 2>&1; then
  info "keel is on PATH in this environment: $(command -v keel)"
else
  info "keel installed; open a new terminal (or: export PATH=\"${INSTALL_DIR}:\$PATH\")"
fi

info "done."
printf '\nNext steps:\n'
printf '  # 1. Start the global daemon\n'
printf '  keel daemon                 # leave running in a terminal or supervisor\n'
printf '  # (If you installed via Homebrew instead: brew services start keel)\n'
printf '\n'
printf '  # 2. In each project\n'
printf '  cd /path/to/your/project\n'
printf '  keel start\n'
printf '\n'
printf '  # 3. Cursor MCP (~/.cursor/mcp.json) — absolute binary path:\n'
printf '    "command": "%s/keel"\n' "$INSTALL_DIR"
printf '    "args": ["mcp"]\n'
printf '    # KEEL_INDEX_DB is optional (auto: cwd walk-up, then daemon registry)\n'
printf '\n'
printf '  See README Quick start for the agent rule and verification.\n'
