#!/usr/bin/env bash
#
# CommOS installer — brings a box to a working state fast.
#
# It does the things new users most often get wrong:
#   • auto-detects this host's LAN IP and writes it as `media_ip` (the #1 cause of
#     "call connects but no audio" is a loopback media_ip),
#   • generates a valid pbx.yaml (SQLite durable, zero external dependency),
#   • creates the data directory,
#   • stores an admin password as a file-referenced secret (never inline — CMOS-14-DEP-083),
#   • optionally installs a systemd service,
#   • prints exactly how to add phones and place a test call.
#
# Usage:
#   scripts/install.sh [options]
#
# Options:
#   --media-ip <ip>        RTP/SDP address phones send audio to (default: auto-detected LAN IP)
#   --http-port <port>     API + dashboard port (default: 8080)
#   --sip-port <port>      SIP UDP port (default: 5060)
#   --data-dir <path>      State dir for the SQLite DB, objects, config (default: /var/lib/commos
#                          as root, else ./commos-data)
#   --admin-password <pw>  Enable admin auth; stored as a 0600 file secret, referenced from config
#   --bin <path>           Path to the commosd binary (default: auto-locate on PATH / target dir)
#   --config <path>        Where to write pbx.yaml (default: <data-dir>/pbx.yaml)
#   --build                Build the binary from source with cargo if none is found
#   --systemd              Install and enable a systemd service (needs root)
#   --force                Overwrite an existing pbx.yaml
#   -h, --help             This help
#
set -euo pipefail

# ---- defaults ---------------------------------------------------------------------------
HTTP_PORT=8080
SIP_PORT=5060
MEDIA_IP=""
ADMIN_PASSWORD=""
BIN=""
CONFIG=""
DATA_DIR=""
DO_BUILD=0
DO_SYSTEMD=0
FORCE=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"   # the `reference/` workspace

log()  { printf '\033[1;36m▸\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0; }

# ---- args -------------------------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --media-ip) MEDIA_IP="$2"; shift 2;;
    --http-port) HTTP_PORT="$2"; shift 2;;
    --sip-port) SIP_PORT="$2"; shift 2;;
    --data-dir) DATA_DIR="$2"; shift 2;;
    --admin-password) ADMIN_PASSWORD="$2"; shift 2;;
    --bin) BIN="$2"; shift 2;;
    --config) CONFIG="$2"; shift 2;;
    --build) DO_BUILD=1; shift;;
    --systemd) DO_SYSTEMD=1; shift;;
    --force) FORCE=1; shift;;
    -h|--help) usage;;
    *) die "unknown option: $1 (try --help)";;
  esac
done

is_root() { [ "$(id -u)" = "0" ]; }

# ---- data dir + config path -------------------------------------------------------------
if [ -z "$DATA_DIR" ]; then
  if is_root; then DATA_DIR="/var/lib/commos"; else DATA_DIR="$PWD/commos-data"; fi
fi
[ -z "$CONFIG" ] && CONFIG="$DATA_DIR/pbx.yaml"

# ---- detect LAN IP ----------------------------------------------------------------------
detect_ip() {
  local ip=""
  # Best: the source address the kernel would use to reach the internet.
  ip="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -n1)"
  [ -n "$ip" ] && { echo "$ip"; return; }
  # Fallback: first non-loopback address hostname knows about.
  ip="$(hostname -I 2>/dev/null | tr ' ' '\n' | grep -vE '^127\.' | head -n1)"
  [ -n "$ip" ] && { echo "$ip"; return; }
  echo ""
}

if [ -z "$MEDIA_IP" ]; then
  MEDIA_IP="$(detect_ip)"
  if [ -n "$MEDIA_IP" ]; then
    ok "detected LAN IP: $MEDIA_IP  (phones will send audio here)"
  else
    die "could not auto-detect a LAN IP — pass one with --media-ip <ip>"
  fi
fi
case "$MEDIA_IP" in
  127.*|::1|localhost) warn "media_ip is loopback ($MEDIA_IP): real phones will get NO AUDIO. Pass --media-ip <LAN-IP>.";;
esac

# ---- locate / build the binary ----------------------------------------------------------
if [ -z "$BIN" ]; then
  if command -v commosd >/dev/null 2>&1; then
    BIN="$(command -v commosd)"
  elif [ -x "$REPO_DIR/target/release/commosd" ]; then
    BIN="$REPO_DIR/target/release/commosd"
  elif [ -x "$REPO_DIR/target/debug/commosd" ]; then
    BIN="$REPO_DIR/target/debug/commosd"
  fi
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  if [ "$DO_BUILD" = "1" ] && command -v cargo >/dev/null 2>&1; then
    log "building commosd (release) — this can take a few minutes…"
    ( cd "$REPO_DIR" && cargo build --release --bin commosd )
    BIN="$REPO_DIR/target/release/commosd"
  else
    die "commosd binary not found. Provide --bin <path>, put it on PATH, or pass --build (needs cargo)."
  fi
fi
ok "using binary: $BIN"

# ---- create data dir --------------------------------------------------------------------
mkdir -p "$DATA_DIR"
ok "data dir: $DATA_DIR"

# ---- admin password (as a file-referenced secret, never inline) -------------------------
ADMIN_YAML=""
if [ -n "$ADMIN_PASSWORD" ]; then
  PW_FILE="$DATA_DIR/admin_password"
  printf '%s' "$ADMIN_PASSWORD" > "$PW_FILE"
  chmod 600 "$PW_FILE"
  ADMIN_YAML=$'admin_password:\n  ref_uri: "file://'"$PW_FILE"$'"'
  ok "admin auth enabled (secret at $PW_FILE, referenced from config)"
fi

# ---- write pbx.yaml ---------------------------------------------------------------------
if [ -e "$CONFIG" ] && [ "$FORCE" != "1" ]; then
  die "config already exists: $CONFIG (use --force to overwrite)"
fi

{
  echo "# CommOS configuration — generated by scripts/install.sh"
  echo "# The single binary boots on embedded SQLite (durable, zero external dependency)."
  echo "listen: \"0.0.0.0:$HTTP_PORT\""
  echo "sip_listen: \"0.0.0.0:$SIP_PORT\""
  echo "# media_ip is the address phones send RTP audio to — MUST be reachable from the phones."
  echo "media_ip: \"$MEDIA_IP\""
  echo "data_dir: \"$DATA_DIR\""
  [ -n "$ADMIN_YAML" ] && echo "$ADMIN_YAML"
} > "$CONFIG"
ok "wrote config: $CONFIG"

# ---- optional systemd service -----------------------------------------------------------
if [ "$DO_SYSTEMD" = "1" ]; then
  is_root || die "--systemd needs root (re-run with sudo)"
  UNIT=/etc/systemd/system/commosd.service
  cat > "$UNIT" <<UNITEOF
[Unit]
Description=CommOS communications platform (commosd)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN --config $CONFIG
Restart=on-failure
RestartSec=2
# Systemd sends SIGTERM; commosd drains gracefully (readiness off, then finishes in-flight).
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
UNITEOF
  systemctl daemon-reload
  systemctl enable --now commosd.service
  ok "systemd service installed and started (systemctl status commosd)"
fi

# ---- next steps -------------------------------------------------------------------------
echo
ok "CommOS is configured."
echo
if [ "$DO_SYSTEMD" != "1" ]; then
  log "Start it:"
  echo "    $BIN --config $CONFIG"
  echo
fi
log "Then, from any machine on the LAN:"
echo "    • Onboard phones:   http://$MEDIA_IP:$HTTP_PORT/onboarding"
echo "    • Live dashboard:   http://$MEDIA_IP:$HTTP_PORT/dashboard"
echo "    • Health/metrics:   http://$MEDIA_IP:$HTTP_PORT/livez   http://$MEDIA_IP:$HTTP_PORT/metrics"
echo
log "Point each phone's SIP account at:"
echo "    • Server / registrar: $MEDIA_IP:$SIP_PORT   (UDP)"
echo "    • Username:           the extension number you assigned in the wizard"
echo "    • (Auto-provision:    set DHCP option 66 to http://$MEDIA_IP:$HTTP_PORT/provision)"
echo
log "Test the call path:"
echo "    • Dial your own number  → echo test (you hear yourself = signalling + audio OK)"
echo "    • Dial another phone's extension → two-way call"
echo
warn "This is a LAN test bed: SIP/RTP are unencrypted and REGISTER is not yet authenticated —"
warn "do NOT expose UDP $SIP_PORT to the internet. Open $SIP_PORT/udp and the RTP range to phones only."
