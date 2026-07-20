#!/usr/bin/env bash
# Mock-release test for install.sh (no GitHub network required).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"; kill ${SERVER_PID:-0} 2>/dev/null || true' EXIT

command -v sh >/dev/null
command -v python3 >/dev/null
sh -n "$ROOT/install.sh"

VERSION="1.1.0"
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in Darwin) OS_KEY=apple-darwin ;; Linux) OS_KEY=unknown-linux-gnu ;; *)
  echo "install test skipped on $OS"; exit 0 ;;
esac
case "$ARCH" in x86_64|amd64) ARCH_KEY=x86_64 ;; arm64|aarch64) ARCH_KEY=aarch64 ;; *)
  echo "install test skipped on $ARCH"; exit 0 ;;
esac
TARGET="${ARCH_KEY}-${OS_KEY}"
ARCHIVE="keel-${VERSION}-${TARGET}.tar.gz"

mkdir -p "$TMP/payload" "$TMP/bin" "$TMP/www"
printf '#!/bin/sh\necho keel-mock-ok\n' > "$TMP/payload/keel"
chmod +x "$TMP/payload/keel"
printf 'readme\n' > "$TMP/payload/README.md"
printf 'changelog\n' > "$TMP/payload/CHANGELOG.md"
tar -czf "$TMP/www/${ARCHIVE}" -C "$TMP/payload" .
(
  cd "$TMP/www"
  if command -v shasum >/dev/null; then
    shasum -a 256 "$ARCHIVE" > SHA256SUMS
  else
    sha256sum "$ARCHIVE" > SHA256SUMS
  fi
)

PORT=18765
python3 - "$TMP/www" "$VERSION" "$PORT" <<'PY' &
import http.server, os, sys
root, version, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
os.chdir(root)

class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split("?")[0]
        if path.endswith("/releases/latest"):
            body = f'{{"tag_name":"v{version}"}}'.encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path.endswith("SHA256SUMS"):
            self.path = "/SHA256SUMS"
        elif path.endswith(".tar.gz"):
            self.path = "/" + path.rsplit("/", 1)[-1]
        return super().do_GET()

    def log_message(self, *_args):
        pass

http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
SERVER_PID=$!
sleep 0.5

# Rewrite GitHub hosts to the mock server for this run.
MOCK_INSTALL="$TMP/install.sh"
sed \
  -e "s|https://api.github.com|http://127.0.0.1:${PORT}|g" \
  -e "s|https://github.com/|http://127.0.0.1:${PORT}/|g" \
  "$ROOT/install.sh" > "$MOCK_INSTALL"

KEEL_VERSION="v${VERSION}" \
KEEL_INSTALL_DIR="$TMP/bin" \
  sh "$MOCK_INSTALL"

test -x "$TMP/bin/keel"
test "$("$TMP/bin/keel")" = "keel-mock-ok"
echo "OK: install.sh mock release test passed"
