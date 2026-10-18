#!/usr/bin/env bash
# Authenticated notebook + blob round-trip against a live Ignite Server.
#
# Uses product APIs only (pairing → PUT blob → POST library → POST operations → GET).
# Secrets live only in 0600 files under SCRATCH and reach curl as `-H @file`, so they never
# appear in argv, the environment, stdout/stderr, or EVIDENCE (which is sanitized).
#
# Required env:
#   BASE_URL          e.g. http://127.0.0.1:8787
#   FIXTURES_DIR      path containing fixture-manifest.json + blobs/ + snapshots
#   EVIDENCE          sanitized evidence directory
#   SCRATCH           private temp directory (wiped by caller)
#   ADMIN_COOKIE_FILE (seed only) file holding one line `scribe_session=…`
#
# Optional:
#   PHASE_LABEL       label written into evidence filenames (default: initial)
#   MODE              seed (default) | verify
#                     verify skips pairing/PUT/POST and reads with the existing bearer
#   BEARER_FILE       bearer credential file (default: $SCRATCH/bearer.token);
#                     seed writes it (0600) after pairing, verify reads it
#
# Exits 0 on success. In seed mode, prints LIBRARY_ID=… / BLOB_SHA=… / DEVICE_ID=… on stdout.
# The bearer is never printed.
set -euo pipefail
umask 077

die() { echo "api-notebook-blob-roundtrip: ERROR: $*" >&2; exit 1; }
say() { echo "api-notebook-blob-roundtrip: $*"; }

BASE_URL="${BASE_URL:?}"
FIXTURES_DIR="${FIXTURES_DIR:?}"
EVIDENCE="${EVIDENCE:?}"
SCRATCH="${SCRATCH:?}"
PHASE_LABEL="${PHASE_LABEL:-initial}"
MODE="${MODE:-seed}"
BASE_URL="${BASE_URL%/}"
BEARER_FILE="${BEARER_FILE:-$SCRATCH/bearer.token}"
AUTH_HDR="$SCRATCH/auth.header"
COOKIE_HDR="$SCRATCH/cookie.header"

command -v curl >/dev/null 2>&1 || die "curl required"
command -v python3 >/dev/null 2>&1 || die "python3 required for JSON"

case "$MODE" in
  seed)
    ADMIN_COOKIE_FILE="${ADMIN_COOKIE_FILE:?}"
    [ -s "$ADMIN_COOKIE_FILE" ] || die "ADMIN_COOKIE_FILE is missing or empty"
    ;;
  verify) [ -s "$BEARER_FILE" ] || die "verify needs an existing bearer file (BEARER_FILE)" ;;
  *) die "MODE must be seed or verify (got $MODE)" ;;
esac

MANIFEST="$FIXTURES_DIR/fixture-manifest.json"
[ -f "$MANIFEST" ] || die "missing $MANIFEST"

sanitize() {
  sed -E \
    -e 's/setup_code=[A-Z0-9-]+/setup_code=<redacted>/g' \
    -e 's/[Ss]et-[Cc]ookie:[^\r\n]+/Set-Cookie: <redacted>/g' \
    -e 's/scribe_session=[^;[:space:]]+/scribe_session=<redacted>/g' \
    -e 's/[Aa]uthorization:[[:space:]]*Bearer[[:space:]]+[^[:space:]]+/Authorization: Bearer <redacted>/g' \
    -e 's/"setup_code"[[:space:]]*:[[:space:]]*"[^"]+"/"setup_code":"<redacted>"/g' \
    -e 's/"request_secret"[[:space:]]*:[[:space:]]*"[^"]+"/"request_secret":"<redacted>"/g' \
    -e 's/"credential"[[:space:]]*:[[:space:]]*"[^"]+"/"credential":"<redacted>"/g' \
    -e 's/"token"[[:space:]]*:[[:space:]]*"[^"]+"/"token":"<redacted>"/g' \
    -e 's/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.[A-Za-z0-9_-]{16,}/<redacted-credential>/g'
}

json_field() {
  # Usage: json_field <file> <dotted.path>
  local file="$1" path="$2"
  python3 - "$file" "$path" <<'PY'
import json, sys
path = sys.argv[2].split(".")
with open(sys.argv[1], encoding="utf-8") as f:
    d = json.load(f)
for p in path:
    if isinstance(d, dict):
        d = d[p]
    else:
        raise SystemExit(f"cannot traverse {p} into {type(d).__name__}")
if isinstance(d, (dict, list)):
    print(json.dumps(d, separators=(",", ":")))
elif d is None:
    print("")
else:
    print(d)
PY
}

# Copy one JSON string field from a response file into a 0600 file, never through a shell
# variable. Usage: json_secret_to_file <src> <field> <dst> [json-key-for-object-output]
json_secret_to_file() {
  python3 - "$@" <<'PY'
import json, sys
src, field, dst = sys.argv[1], sys.argv[2], sys.argv[3]
key = sys.argv[4] if len(sys.argv) > 4 else ""
try:
    val = json.load(open(src, encoding="utf-8")).get(field) or ""
except Exception:
    val = ""
if not isinstance(val, str) or not val:
    raise SystemExit(1)
with open(dst, "w", encoding="utf-8") as f:
    if key:
        json.dump({key: val}, f)
    else:
        f.write(val + "\n")
PY
}

write_auth_header() {
  printf 'Authorization: Bearer %s\n' "$(tr -d '\r\n' <"$BEARER_FILE")" >"$AUTH_HDR"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
  fi
}

LIBRARY_ID="$(json_field "$MANIFEST" library_id)"
BLOB_REL="$(json_field "$MANIFEST" blob.file)"
BLOB_SHA="$(json_field "$MANIFEST" blob.sha256)"
SNAPSHOT_REL="$(json_field "$MANIFEST" snapshot)"
LIVE_OPS_REL="$(json_field "$MANIFEST" live_ops)"
BLOB_PATH="$FIXTURES_DIR/$BLOB_REL"
SNAPSHOT_PATH="$FIXTURES_DIR/$SNAPSHOT_REL"
LIVE_OPS_PATH="$FIXTURES_DIR/$LIVE_OPS_REL"

[ -n "$LIBRARY_ID" ] || die "manifest missing library_id"
[ -f "$BLOB_PATH" ] || die "missing blob $BLOB_PATH"
[ -f "$SNAPSHOT_PATH" ] || die "missing snapshot $SNAPSHOT_PATH"
[ -f "$LIVE_OPS_PATH" ] || die "missing live ops $LIVE_OPS_PATH"
ACTUAL_BLOB_SHA="$(sha256_file "$BLOB_PATH")"
[ "$ACTUAL_BLOB_SHA" = "$BLOB_SHA" ] || die "fixture blob sha mismatch ($ACTUAL_BLOB_SHA != $BLOB_SHA)"

DEVICE_ID=""

verify_reads() {
  local label="$1"
  local AUTH=(-H "@$AUTH_HDR")
  say "verify reads ($label)"

  META_HTTP="$(curl -sS -o "$SCRATCH/lib-meta.body" -w '%{http_code}' \
    "${AUTH[@]}" "$BASE_URL/v1/libraries/${LIBRARY_ID}" || true)"
  [ "$META_HTTP" = "200" ] || die "GET library meta HTTP $META_HTTP"
  # Persist ids/seqs only — not notebook payload.
  python3 - "$SCRATCH/lib-meta.body" "$EVIDENCE/lib-meta.${label}.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
out = {k: d.get(k) for k in ("library_id", "genesis_sha256", "head_server_seq") if k in d}
json.dump(out, open(sys.argv[2], "w", encoding="utf-8"), indent=2)
open(sys.argv[2], "a", encoding="utf-8").write("\n")
PY
  GOT_ID="$(json_field "$SCRATCH/lib-meta.body" library_id)"
  [ "$GOT_ID" = "$LIBRARY_ID" ] || die "library_id mismatch ($GOT_ID)"
  HEAD_SEQ="$(json_field "$SCRATCH/lib-meta.body" head_server_seq)"
  [ -n "$HEAD_SEQ" ] || die "missing head_server_seq"
  python3 -c "import sys; n=int(sys.argv[1]); sys.exit(0 if n>=2 else 1)" "$HEAD_SEQ" \
    || die "expected head_server_seq>=2, got $HEAD_SEQ"

  SNAP_HTTP="$(curl -sS -o "$SCRATCH/lib-snap.body" -w '%{http_code}' \
    "${AUTH[@]}" "$BASE_URL/v1/libraries/${LIBRARY_ID}/snapshot" || true)"
  [ "$SNAP_HTTP" = "200" ] || die "GET snapshot HTTP $SNAP_HTTP"
  SNAP_SHA_GOT="$(json_field "$SCRATCH/lib-snap.body" snapshot_sha256)"
  THROUGH="$(json_field "$SCRATCH/lib-snap.body" through_server_seq)"
  python3 - "$MANIFEST" "$SNAP_SHA_GOT" <<'PY' || die "snapshot integrity check failed"
import json, sys
m = json.load(open(sys.argv[1], encoding="utf-8"))
got = sys.argv[2]
post = set(m.get("snapshot_sha256_after_live_ops") or [])
if not post:
    raise SystemExit("manifest missing snapshot_sha256_after_live_ops")
if got not in post:
    raise SystemExit(f"snapshot_sha256 {got} not in post-live-ops set {sorted(post)}")
PY
  {
    echo "snapshot_sha256=$SNAP_SHA_GOT"
    echo "through_server_seq=$THROUGH"
    echo "library_id=$LIBRARY_ID"
    echo "head_server_seq=$HEAD_SEQ"
  } >"$EVIDENCE/lib-snapshot.${label}.status.txt"
  rm -f "$SCRATCH/lib-snap.body"

  OPS_GET_HTTP="$(curl -sS -o "$SCRATCH/ops-get.body" -w '%{http_code}' \
    "${AUTH[@]}" "$BASE_URL/v1/libraries/${LIBRARY_ID}/operations?after=0&limit=100" || true)"
  [ "$OPS_GET_HTTP" = "200" ] || die "GET operations HTTP $OPS_GET_HTTP"
  OP_COUNT="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1],encoding="utf-8")); print(len(d.get("operations") or []))' "$SCRATCH/ops-get.body")"
  [ "$OP_COUNT" -ge 2 ] || die "expected >=2 operations, got $OP_COUNT"
  # Counts/seqs only — operation envelopes contain stroke geometry.
  {
    echo "operations_count=$OP_COUNT"
    echo "head_server_seq=$(json_field "$SCRATCH/ops-get.body" head_server_seq)"
  } >"$EVIDENCE/ops-get.${label}.status.txt"

  BLOB_GET_HTTP="$(curl -sS -o "$SCRATCH/blob-get.bin" -w '%{http_code}' \
    "${AUTH[@]}" "$BASE_URL/v1/blobs/${BLOB_SHA}" || true)"
  [ "$BLOB_GET_HTTP" = "200" ] || die "GET blob HTTP $BLOB_GET_HTTP"
  GOT_BLOB_SHA="$(sha256_file "$SCRATCH/blob-get.bin")"
  [ "$GOT_BLOB_SHA" = "$BLOB_SHA" ] || die "blob payload sha mismatch"
  echo "blob_get_http=200 sha256=$GOT_BLOB_SHA bytes=$(wc -c <"$SCRATCH/blob-get.bin" | tr -d ' ')" \
    >"$EVIDENCE/blob-get.${label}.status.txt"
  rm -f "$SCRATCH/blob-get.bin" "$SCRATCH/lib-meta.body" "$SCRATCH/ops-get.body" \
    "$SCRATCH/blob-put.body" "$SCRATCH/lib-post.body" "$SCRATCH/ops-post.body"
}

if [ "$MODE" = "seed" ]; then
  printf 'Cookie: %s\n' "$(tr -d '\r\n' <"$ADMIN_COOKIE_FILE")" >"$COOKIE_HDR"
  HDR_ADMIN=(
    -H "@$COOKIE_HDR"
    -H "X-Scribe-Request: 1"
    -H "Content-Type: application/json"
  )

  say "pairing Kindle client ($PHASE_LABEL)"
  PAIR_BODY="$SCRATCH/pair-start.json"
  printf '{"client_type":"kindle","display_name":"accept-docker-%s","client_id":"accept-%s"}\n' \
    "$PHASE_LABEL" "$PHASE_LABEL" >"$PAIR_BODY"
  PAIR_HTTP="$(curl -sS -o "$SCRATCH/pair-start.body" -w '%{http_code}' \
    -X POST "$BASE_URL/control/v1/pairing" \
    "${HDR_ADMIN[@]}" \
    --data-binary @"$PAIR_BODY" || true)"
  sanitize <"$SCRATCH/pair-start.body" >"$EVIDENCE/pair-start.${PHASE_LABEL}.body.txt"
  [ "$PAIR_HTTP" = "201" ] || [ "$PAIR_HTTP" = "200" ] \
    || die "pairing start HTTP $PAIR_HTTP"
  REQUEST_ID="$(json_field "$SCRATCH/pair-start.body" request_id)"
  [ -n "$REQUEST_ID" ] || die "pairing start missing request_id"
  POLL_JSON="$SCRATCH/pair-poll.json"
  json_secret_to_file "$SCRATCH/pair-start.body" request_secret "$POLL_JSON" request_secret \
    || die "pairing start missing request_secret"
  rm -f "$PAIR_BODY"

  APPROVE_HTTP="$(curl -sS -o "$SCRATCH/pair-approve.body" -w '%{http_code}' \
    -X POST "$BASE_URL/control/v1/admin/pairing/${REQUEST_ID}/approve" \
    "${HDR_ADMIN[@]}" || true)"
  sanitize <"$SCRATCH/pair-approve.body" >"$EVIDENCE/pair-approve.${PHASE_LABEL}.body.txt"
  [ "$APPROVE_HTTP" = "200" ] || die "pairing approve HTTP $APPROVE_HTTP"

  POLL_HTTP="$(curl -sS -o "$SCRATCH/pair-poll.body" -w '%{http_code}' \
    -X POST "$BASE_URL/control/v1/pairing/${REQUEST_ID}/poll" \
    -H "X-Scribe-Request: 1" \
    -H "Content-Type: application/json" \
    --data-binary @"$POLL_JSON" || true)"
  sanitize <"$SCRATCH/pair-poll.body" >"$EVIDENCE/pair-poll.${PHASE_LABEL}.body.txt"
  rm -f "$POLL_JSON"
  [ "$POLL_HTTP" = "200" ] || die "pairing poll HTTP $POLL_HTTP"
  json_secret_to_file "$SCRATCH/pair-poll.body" credential "$BEARER_FILE" \
    || die "pairing poll missing credential"
  chmod 600 "$BEARER_FILE"
  DEVICE_ID="$(json_field "$SCRATCH/pair-poll.body" device_id)"
  echo "pairing_ok=1 device_id_present=$([ -n "$DEVICE_ID" ] && echo yes || echo no)" \
    >"$EVIDENCE/pairing.${PHASE_LABEL}.status.txt"
  rm -f "$SCRATCH/pair-start.body" "$SCRATCH/pair-approve.body" "$SCRATCH/pair-poll.body" \
    "$COOKIE_HDR"
  unset REQUEST_ID

  write_auth_header
  AUTH=(-H "@$AUTH_HDR")

  say "PUT blob ($PHASE_LABEL)"
  BLOB_HTTP="$(curl -sS -o "$SCRATCH/blob-put.body" -w '%{http_code}' \
    -X PUT "$BASE_URL/v1/blobs/${BLOB_SHA}" \
    "${AUTH[@]}" \
    --data-binary @"$BLOB_PATH" || true)"
  echo "blob_put_http=$BLOB_HTTP sha256=$BLOB_SHA" >"$EVIDENCE/blob-put.${PHASE_LABEL}.status.txt"
  case "$BLOB_HTTP" in
    200|201|204|409) ;;
    *) die "blob put HTTP $BLOB_HTTP" ;;
  esac

  say "POST library genesis ($PHASE_LABEL)"
  LIB_HTTP="$(curl -sS -o "$SCRATCH/lib-post.body" -w '%{http_code}' \
    -X POST "$BASE_URL/v1/libraries" \
    "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    --data-binary @"$SNAPSHOT_PATH" || true)"
  echo "libraries_post_http=$LIB_HTTP library_id=$LIBRARY_ID" \
    >"$EVIDENCE/lib-post.${PHASE_LABEL}.status.txt"
  case "$LIB_HTTP" in
    200|201) ;;
    409) say "library already present (HTTP 409) — continuing if GET succeeds" ;;
    *) die "libraries post HTTP $LIB_HTTP" ;;
  esac

  say "POST live operations ($PHASE_LABEL)"
  OPS_HTTP="$(curl -sS -o "$SCRATCH/ops-post.body" -w '%{http_code}' \
    -X POST "$BASE_URL/v1/libraries/${LIBRARY_ID}/operations" \
    "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    --data-binary @"$LIVE_OPS_PATH" || true)"
  echo "operations_post_http=$OPS_HTTP" >"$EVIDENCE/ops-post.${PHASE_LABEL}.status.txt"
  case "$OPS_HTTP" in
    200|201) ;;
    *) die "operations post HTTP $OPS_HTTP" ;;
  esac
else
  write_auth_header
fi

verify_reads "$PHASE_LABEL"
rm -f "$AUTH_HDR"

if [ "$MODE" = "seed" ]; then
  printf 'LIBRARY_ID=%s\n' "$LIBRARY_ID"
  printf 'BLOB_SHA=%s\n' "$BLOB_SHA"
  printf 'DEVICE_ID=%s\n' "$DEVICE_ID"
fi
say "PASS ($PHASE_LABEL mode=$MODE)"
