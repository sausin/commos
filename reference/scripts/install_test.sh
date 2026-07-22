#!/usr/bin/env bash
# CommOS reference — installer (scripts/install.sh) test.
#
# The installer is a bash script, so its failure modes (a dead sounds mirror, a SIGPIPE in
# password generation, a partially-written config) are invisible to `cargo test`. This exercises
# the paths that have historically broken a fresh install, all offline: no network, no cargo, no
# root — a fake `commosd` binary stands in for the real one and `file://` URLs stand in for the
# sounds mirrors.
#
# What it guarantees:
#   • a fresh install always writes a valid pbx.yaml and (unless opted out) an admin password —
#     even when BOTH sound mirrors 404 or the archive is corrupt (the sounds step is best-effort),
#   • auto-generating the admin password never aborts the installer (the tr|head SIGPIPE trap),
#   • the sounds download tries the fallback mirror, extracts a good pack, and cleans up its temp,
#   • --no-admin-password / explicit --admin-password / --force behave as documented.
#
# Usage:
#   scripts/install_test.sh          # exits non-zero on the first failed assertion group
#
# Not wired to a running daemon — it only inspects what the installer writes to disk + prints.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL="$SCRIPT_DIR/install.sh"
[ -x "$INSTALL" ] || { echo "cannot find install.sh next to this test: $INSTALL" >&2; exit 2; }

PASS=0
FAIL=0
green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }
pass()  { PASS=$((PASS + 1)); printf '  [%s] %s\n' "$(green PASS)" "$1"; }
fail()  { FAIL=$((FAIL + 1)); printf '  [%s] %s\n' "$(red FAIL)" "$1"; }

# assert helpers ---------------------------------------------------------------------------
check()      { if [ "$1" = "0" ] || [ "$1" = "true" ]; then pass "$2"; else fail "$2"; fi; }
assert_eq()  { if [ "$1" = "$2" ]; then pass "$3"; else fail "$3 (got '$1', want '$2')"; fi; }
assert_file(){ if [ -f "$1" ]; then pass "$2"; else fail "$2 (missing: $1)"; fi; }
assert_nofile(){ if [ ! -e "$1" ]; then pass "$2"; else fail "$2 (unexpected: $1)"; fi; }
assert_grep(){ if grep -qF "$1" "$2" 2>/dev/null; then pass "$3"; else fail "$3 (no '$1' in $2)"; fi; }
assert_nogrep(){ if grep -qF "$1" "$2" 2>/dev/null; then fail "$3 (unexpected '$1' in $2)"; else pass "$3"; fi; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/commos-install-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# A fake commosd so the installer doesn't need a real build / cargo.
FAKE_BIN="$WORK/commosd"
printf '#!/bin/sh\necho "commosd 0.0.0-test"\n' > "$FAKE_BIN"
chmod +x "$FAKE_BIN"

# A valid Asterisk-style sound pack (files at the archive root, incl. the vm-intro sentinel).
PACKSRC="$WORK/packsrc"; mkdir -p "$PACKSRC/digits"
printf 'AUDIO' > "$PACKSRC/vm-intro.ulaw"
printf 'AUDIO' > "$PACKSRC/vm-youhave.ulaw"
printf 'AUDIO' > "$PACKSRC/digits/1.ulaw"
GOOD_PACK="$WORK/good-sounds.tar.gz"
tar -czf "$GOOD_PACK" -C "$PACKSRC" .
# A file that is NOT a gzip tarball, to exercise the extraction-failure branch.
NOT_A_TARBALL="$WORK/corrupt.tar.gz"
printf 'this is not a gzip archive' > "$NOT_A_TARBALL"

DEAD="https://example.invalid/does-not-exist.tar.gz"

# Run the installer non-interactively; returns its exit code, logs to $2.
run_install() {
  local logf="$1"; shift
  bash "$INSTALL" "$@" </dev/null >"$logf" 2>&1
}

echo "installer test — $INSTALL"
echo

# -- 1. auto-generated password + BOTH sound mirrors dead: install must still fully succeed ----
# This is the regression that motivated the test: a dead mirror, or the SIGPIPE from `tr|head`
# in password generation, used to abort the script (`set -e`), leaving no pbx.yaml + no password.
D="$WORK/case1"; LOG="$WORK/case1.log"
SOUNDS_URL="$DEAD" SOUNDS_URL_FALLBACK="$DEAD" \
  run_install "$LOG" --media-ip 192.168.1.50 --data-dir "$D" --bin "$FAKE_BIN" --sounds
rc=$?
echo "case 1: auto password, both sound mirrors dead"
assert_eq "$rc" "0" "installer exits 0 despite dead sound mirrors"
assert_file "$D/pbx.yaml" "pbx.yaml written"
assert_file "$D/admin_password" "admin_password secret written"
assert_eq "$(wc -c <"$D/admin_password" 2>/dev/null | tr -d ' ')" "24" "generated password is 24 chars"
assert_grep 'ADMIN PASSWORD (generated)' "$LOG" "generated-password banner shown"
assert_grep 'media_ip: "192.168.1.50"' "$D/pbx.yaml" "media_ip persisted to config"
assert_grep "data_dir: \"$D\"" "$D/pbx.yaml" "data_dir persisted to config"
assert_grep 'ref_uri: "file://' "$D/pbx.yaml" "admin password referenced (never inline)"
assert_nogrep "$(cat "$D/admin_password")" "$D/pbx.yaml" "plaintext password NOT inlined in config"
assert_grep 'beep' "$LOG" "warns voicemail falls back to a beep"
assert_nofile "$D/sounds/en/vm-intro.ulaw" "no sound prompts installed (both mirrors dead)"
echo

# -- 2. sounds download succeeds from the primary mirror --------------------------------------
D="$WORK/case2"; LOG="$WORK/case2.log"
SOUNDS_URL="file://$GOOD_PACK" SOUNDS_URL_FALLBACK="" \
  run_install "$LOG" --media-ip 10.0.0.2 --data-dir "$D" --bin "$FAKE_BIN" --sounds --no-admin-password
rc=$?
echo "case 2: sounds from primary mirror"
assert_eq "$rc" "0" "installer exits 0"
assert_file "$D/sounds/en/vm-intro.ulaw" "vm-intro.ulaw installed"
assert_grep 'audio prompts installed' "$LOG" "reports prompts installed"
# The download uses a temp tarball that must be cleaned up regardless of outcome.
check "$([ -z "$(ls "${TMPDIR:-/tmp}"/commos-sounds.* 2>/dev/null)" ] && echo 0 || echo 1)" \
  "temporary sounds tarball cleaned up"
echo

# -- 3. primary mirror dead, fallback good: fallback is tried and wins ------------------------
D="$WORK/case3"; LOG="$WORK/case3.log"
SOUNDS_URL="$DEAD" SOUNDS_URL_FALLBACK="file://$GOOD_PACK" \
  run_install "$LOG" --media-ip 10.0.0.3 --data-dir "$D" --bin "$FAKE_BIN" --sounds --no-admin-password
rc=$?
echo "case 3: primary dead, fallback mirror good"
assert_eq "$rc" "0" "installer exits 0"
assert_file "$D/sounds/en/vm-intro.ulaw" "vm-intro.ulaw installed from fallback"
assert_grep 'mirror unreachable' "$LOG" "reports the primary mirror as unreachable"
echo

# -- 4. downloaded archive is corrupt (not a tarball): non-fatal, install completes -----------
D="$WORK/case4"; LOG="$WORK/case4.log"
SOUNDS_URL="file://$NOT_A_TARBALL" SOUNDS_URL_FALLBACK="" \
  run_install "$LOG" --media-ip 10.0.0.4 --data-dir "$D" --bin "$FAKE_BIN" --sounds --no-admin-password
rc=$?
echo "case 4: corrupt sounds archive"
assert_eq "$rc" "0" "installer exits 0 on a non-tarball download"
assert_file "$D/pbx.yaml" "pbx.yaml still written"
assert_nofile "$D/sounds/en/vm-intro.ulaw" "no bogus prompts left behind"
echo

# -- 5. --no-admin-password: no secret, no config key ----------------------------------------
D="$WORK/case5"; LOG="$WORK/case5.log"
run_install "$LOG" --media-ip 10.0.0.5 --data-dir "$D" --bin "$FAKE_BIN" --no-sounds --no-admin-password
rc=$?
echo "case 5: --no-admin-password"
assert_eq "$rc" "0" "installer exits 0"
assert_file "$D/pbx.yaml" "pbx.yaml written"
assert_nofile "$D/admin_password" "no admin_password secret written"
assert_nogrep 'admin_password:' "$D/pbx.yaml" "no admin_password key in config"
echo

# -- 6. explicit --admin-password stored verbatim as a 0600 file secret ----------------------
D="$WORK/case6"; LOG="$WORK/case6.log"
run_install "$LOG" --media-ip 10.0.0.6 --data-dir "$D" --bin "$FAKE_BIN" --no-sounds --admin-password 'Sup3rSecret!'
rc=$?
echo "case 6: explicit --admin-password"
assert_eq "$rc" "0" "installer exits 0"
assert_file "$D/admin_password" "admin_password secret written"
assert_eq "$(cat "$D/admin_password")" 'Sup3rSecret!' "secret file holds the exact password"
assert_eq "$(stat -c '%a' "$D/admin_password" 2>/dev/null)" "600" "secret file is chmod 600"
assert_nogrep 'ADMIN PASSWORD (generated)' "$LOG" "no generated-password banner for an explicit password"
echo

# -- 7. existing config is protected unless --force ------------------------------------------
D="$WORK/case7"; LOG="$WORK/case7.log"
run_install "$WORK/case7a.log" --media-ip 10.0.0.7 --data-dir "$D" --bin "$FAKE_BIN" --no-sounds --no-admin-password
echo "case 7: existing config protection"
# Second run without --force must refuse (non-zero) and NOT clobber.
run_install "$LOG" --media-ip 10.0.0.99 --data-dir "$D" --bin "$FAKE_BIN" --no-sounds --no-admin-password
rc=$?
check "$([ "$rc" != "0" ] && echo 0 || echo 1)" "refuses to overwrite existing config without --force"
assert_grep 'media_ip: "10.0.0.7"' "$D/pbx.yaml" "existing config left untouched"
# With --force it overwrites.
run_install "$LOG" --media-ip 10.0.0.99 --data-dir "$D" --bin "$FAKE_BIN" --no-sounds --no-admin-password --force
rc=$?
assert_eq "$rc" "0" "overwrites with --force"
assert_grep 'media_ip: "10.0.0.99"' "$D/pbx.yaml" "config overwritten with new value"
echo

# summary ---------------------------------------------------------------------------------
echo "----------------------------------------"
if [ "$FAIL" -eq 0 ]; then
  printf '%s  %d passed, 0 failed\n' "$(green 'INSTALL TEST PASS')" "$PASS"
  exit 0
else
  printf '%s  %d passed, %d failed\n' "$(red 'INSTALL TEST FAIL')" "$PASS" "$FAIL"
  exit 1
fi
