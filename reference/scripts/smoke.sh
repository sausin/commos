#!/usr/bin/env bash
# CommOS reference — end-to-end smoke test.
#
# Exercises the originate-a-call vertical slice against a running commosd: health signals,
# create/get/list on /v1/calls, and the CallStarted event flowing through the outbox
# (observed via the non-normative /_introspect/events ring). Works against either backing
# store — the in-process store or PostgreSQL — since the contract surface is identical.
#
# Usage:
#   scripts/smoke.sh [BASE_URL]        # BASE_URL defaults to http://localhost:8080
#
# Exits non-zero on the first failed assertion.

set -euo pipefail

BASE_URL="${1:-http://localhost:8080}"
BASE_URL="${BASE_URL%/}"   # strip any trailing slash

# Dev bearer token (README): tenant:<uuidv7>. JWT verification is Volume 9 work.
TENANT="01920000-0000-7000-8000-000000000001"
AUTH="Authorization: Bearer tenant:${TENANT}"

PASS=0
FAIL=0

green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }

pass() { PASS=$((PASS + 1)); printf '  [%s] %s\n' "$(green PASS)" "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '  [%s] %s\n' "$(red FAIL)" "$1"; }

die() { fail "$1"; summary; exit 1; }

summary() {
  echo
  echo "----------------------------------------"
  if [ "$FAIL" -eq 0 ]; then
    printf '%s  %d passed, 0 failed  (%s)\n' "$(green SMOKE PASS)" "$PASS" "$BASE_URL"
  else
    printf '%s  %d passed, %d failed  (%s)\n' "$(red 'SMOKE FAIL')" "$PASS" "$FAIL" "$BASE_URL"
  fi
}

# curl wrapper returning "BODY<newline>HTTP_STATUS" — status is the last line.
http() {
  local method="$1"; shift
  local url="$1"; shift
  curl -sS -X "$method" -w $'\n%{http_code}' "$url" "$@"
}

# jq-free JSON field extraction via python3.
json_get() {
  # json_get <expr>  — reads JSON on stdin; `d` is the parsed object.
  python3 -c 'import sys, json; d = json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"
}

command -v curl    >/dev/null 2>&1 || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 2; }

echo "CommOS smoke test against ${BASE_URL}"
echo

# --- 1. Liveness -------------------------------------------------------------
resp="$(http GET "${BASE_URL}/livez")" || die "GET /livez — connection failed"
code="$(tail -n1 <<<"$resp")"
[ "$code" = "200" ] && pass "GET /livez -> 200" || die "GET /livez -> ${code} (expected 200)"

# --- 2. Readiness ------------------------------------------------------------
resp="$(http GET "${BASE_URL}/readyz")" || die "GET /readyz — connection failed"
code="$(tail -n1 <<<"$resp")"
[ "$code" = "200" ] && pass "GET /readyz -> 200" || die "GET /readyz -> ${code} (expected 200)"

# --- 3. Info -----------------------------------------------------------------
resp="$(http GET "${BASE_URL}/info")" || die "GET /info — connection failed"
code="$(tail -n1 <<<"$resp")"
body="$(sed '$d' <<<"$resp")"
if [ "$code" = "200" ]; then
  product="$(json_get 'd["product"]' <<<"$body")"
  version="$(json_get 'd["version"]' <<<"$body")"
  arch="$(json_get 'd["arch"]'    <<<"$body")"
  pass "GET /info -> 200  (product=${product} version=${version} arch=${arch})"
else
  die "GET /info -> ${code} (expected 200)"
fi

# --- 4. Create a call --------------------------------------------------------
payload='{"direction":"OUTBOUND","from_ref":"sip:100","to_ref":"+14155550100"}'
resp="$(http POST "${BASE_URL}/v1/calls" \
  -H "$AUTH" -H 'content-type: application/json' -d "$payload")" \
  || die "POST /v1/calls — connection failed"
code="$(tail -n1 <<<"$resp")"
body="$(sed '$d' <<<"$resp")"
[ "$code" = "201" ] || die "POST /v1/calls -> ${code} (expected 201); body: ${body}"
CALL_ID="$(json_get 'd["id"]' <<<"$body")" || die "POST /v1/calls — response had no id"
[ -n "$CALL_ID" ] || die "POST /v1/calls — empty id"
pass "POST /v1/calls -> 201  (id=${CALL_ID})"

# --- 5. Get the call back ----------------------------------------------------
resp="$(http GET "${BASE_URL}/v1/calls/${CALL_ID}" -H "$AUTH")" \
  || die "GET /v1/calls/{id} — connection failed"
code="$(tail -n1 <<<"$resp")"
body="$(sed '$d' <<<"$resp")"
[ "$code" = "200" ] || die "GET /v1/calls/${CALL_ID} -> ${code} (expected 200)"
got_id="$(json_get 'd["id"]' <<<"$body")"
[ "$got_id" = "$CALL_ID" ] \
  && pass "GET /v1/calls/{id} -> 200  (id matches)" \
  || die "GET /v1/calls/{id} returned id=${got_id} (expected ${CALL_ID})"

# --- 6. List and find the call ----------------------------------------------
resp="$(http GET "${BASE_URL}/v1/calls?limit=200" -H "$AUTH")" \
  || die "GET /v1/calls — connection failed"
code="$(tail -n1 <<<"$resp")"
body="$(sed '$d' <<<"$resp")"
[ "$code" = "200" ] || die "GET /v1/calls -> ${code} (expected 200)"
present="$(python3 -c '
import sys, json
d = json.load(sys.stdin)
print(any(c["id"] == sys.argv[1] for c in d["items"]))
' "$CALL_ID" <<<"$body")"
[ "$present" = "True" ] \
  && pass "GET /v1/calls -> 200  (call present in list)" \
  || die "GET /v1/calls — call ${CALL_ID} not found in items"

# --- 7. CallStarted event in the outbox --------------------------------------
resp="$(http GET "${BASE_URL}/_introspect/events" -H "$AUTH")" \
  || die "GET /_introspect/events — connection failed"
code="$(tail -n1 <<<"$resp")"
body="$(sed '$d' <<<"$resp")"
[ "$code" = "200" ] || die "GET /_introspect/events -> ${code} (expected 200)"
found="$(python3 -c '
import sys, json
events = json.load(sys.stdin)
cid = sys.argv[1]
print(any(e.get("type") == "CallStarted" and e.get("subject") == cid for e in events))
' "$CALL_ID" <<<"$body")"
[ "$found" = "True" ] \
  && pass "GET /_introspect/events -> CallStarted (subject=${CALL_ID}) present" \
  || die "GET /_introspect/events — no CallStarted with subject ${CALL_ID}"

summary
exit 0
