#!/usr/bin/env bash
# Static checks on an assembled Ignite Server Linux bundle (no install).
#   deployment/linux/test-bundle.sh <bundle-dir>
set -euo pipefail
BUNDLE="$(cd "$1" && pwd)"
fail() { echo "BUNDLE FAIL: $*" >&2; exit 1; }

[ -x "$BUNDLE/scribe-server" ] || fail "binary missing/not executable"
[ -x "$BUNDLE/install.sh" ] || fail "install.sh missing"
[ -x "$BUNDLE/uninstall.sh" ] || fail "uninstall.sh missing"
[ -f "$BUNDLE/firewall.sh" ] || fail "firewall.sh missing"
[ -f "$BUNDLE/path-safety.sh" ] || fail "path-safety.sh missing"
[ -f "$BUNDLE/scribe-server.service" ] || fail "unit missing"
[ -f "$BUNDLE/web/index.html" ] || fail "web/index.html missing"
[ -f "$BUNDLE/web/manifest.webmanifest" ] || fail "manifest missing"
[ ! -e "$BUNDLE/scribe-deploy" ] || fail "must not ship scribe-deploy"
[ ! -e "$BUNDLE/deploy-bootstrap.sh" ] || fail "must not ship deploy-bootstrap.sh"

VER="$("$BUNDLE/scribe-server" --version | tr -d '[:space:]')"
[ -n "$VER" ] || fail "binary --version empty"
echo "$VER" | grep -Eq '^[0-9]+\.[0-9]+' || fail "version not semver-like: $VER"

grep -q '"id": "/ignite-web"' "$BUNDLE/web/manifest.webmanifest" || fail "manifest id"
grep -q '"name": "ignite-web"' "$BUNDLE/web/manifest.webmanifest" || fail "manifest name"
[ -n "$(find "$BUNDLE/web" -name '*.wasm' -type f | head -1)" ] || fail "no wasm in web/"
if find "$BUNDLE" \( -type l -o -type b -o -type c -o -type p -o -type s \) | grep -q .; then
  fail "bundle contains symlink or special file"
fi
grep -Eqi 'setup_code|Bearer |scribe_session=' "$BUNDLE/web/index.html" && fail "secret-looking string in index.html"
grep -q "Ignite Server" "$BUNDLE/README.txt" || fail "README branding"

echo "BUNDLE OK version=$VER"
