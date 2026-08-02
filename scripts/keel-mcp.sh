#!/bin/bash
# Cursor MCP launcher for this repo.
# Prefers the newest workspace-built keel so agents see local fixes;
# falls back to Homebrew / PATH.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export KEEL_INDEX_DB="${KEEL_INDEX_DB:-$ROOT/.keel/index.db}"

resolve_keel() {
  if [[ -n "${KEEL_BIN:-}" && -x "${KEEL_BIN}" ]]; then
    printf '%s\n' "$KEEL_BIN"
    return
  fi

  local newest="" c
  for c in \
    "$ROOT/keel/target/release/keel" \
    "$ROOT/keel/target/debug/keel"
  do
    if [[ -x "$c" ]]; then
      if [[ -z "$newest" || "$c" -nt "$newest" ]]; then
        newest="$c"
      fi
    fi
  done
  if [[ -n "$newest" ]]; then
    printf '%s\n' "$newest"
    return
  fi

  for c in /opt/homebrew/bin/keel /usr/local/bin/keel; do
    if [[ -x "$c" ]]; then
      printf '%s\n' "$c"
      return
    fi
  done
  if command -v keel >/dev/null 2>&1; then
    command -v keel
    return
  fi
  echo "keel-mcp.sh: no keel binary found. Build with: (cd \"$ROOT/keel\" && cargo build)" >&2
  echo "Or set KEEL_BIN to an absolute path." >&2
  exit 1
}

KEEL="$(resolve_keel)"
if [[ "${KEEL_MCP_DEBUG:-}" == "1" ]]; then
  echo "keel-mcp.sh: using $KEEL" >&2
fi

# Auto-index on query so a stale .keel/ after local upgrades still refreshes.
# Set KEEL_MCP_NO_AUTO_INDEX=1 to force --no-auto-index (e.g. daemon-watched repos).
if [[ "${KEEL_MCP_NO_AUTO_INDEX:-}" == "1" ]]; then
  exec "$KEEL" --no-auto-index mcp
else
  exec "$KEEL" mcp
fi
