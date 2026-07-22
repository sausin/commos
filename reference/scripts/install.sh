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
#   --tls                  Enable SIP-over-TLS (SIPS). With no --tls-cert, a self-signed cert is
#                          generated (openssl required); builds the binary with --features tls.
#   --tls-cert <path>      Use this PEM certificate chain for SIPS (e.g. a Let's Encrypt
#                          fullchain.pem). Implies --tls.
#   --tls-key <path>       PEM private key for --tls-cert (e.g. Let's Encrypt privkey.pem). Stored
#                          0600 and referenced from config. Implies --tls.
#   --sip-tls-port <port>  SIPS (SIP/TLS) port (default: 5061)
#   --bin <path>           Path to the commosd binary (default: auto-locate on PATH / target dir)
#   --config <path>        Where to write pbx.yaml (default: <data-dir>/pbx.yaml)
#   --sounds               Download the FreePBX audio prompt pack without prompting (voicemail
#                          greeting, retrieval menu). Files are fetched to <data-dir>/sounds and
#                          remain the property of FreePBX/Sangoma. Skipped if already present.
#   --no-sounds            Do not download the audio prompt pack (voicemail falls back to a beep).
#   --build                Build the binary from source with cargo if none is found
#   --systemd              Install and enable a systemd service (needs root)
#   --force                Overwrite an existing pbx.yaml
#   -h, --help             This help
#
set -euo pipefail

# ---- defaults ---------------------------------------------------------------------------
HTTP_PORT=8080
SIP_PORT=5060
SIP_TLS_PORT=5061
MEDIA_IP=""
ADMIN_PASSWORD=""
BIN=""
CONFIG=""
DATA_DIR=""
DO_BUILD=0
DO_SYSTEMD=0
DO_TLS=0
TLS_CERT=""
TLS_KEY=""
FORCE=0
# Sounds: "" = ask (interactive) / attempt (non-interactive), 1 = yes, 0 = no.
DO_SOUNDS=""
# FreePBX's publicly-downloadable G.711 μ-law core sound pack. These prompts are the work of
# FreePBX/Sangoma; we only fetch them onto the operator's system (we do not redistribute them).
# See https://www.freepbx.org/ and https://github.com/FreePBX. Override the mirror if needed.
SOUNDS_URL="${SOUNDS_URL:-https://downloads.freepbx.org/sounds/core-sounds-en-ulaw.tar.gz}"

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
    --tls) DO_TLS=1; shift;;
    --tls-cert) TLS_CERT="$2"; DO_TLS=1; shift 2;;
    --tls-key) TLS_KEY="$2"; DO_TLS=1; shift 2;;
    --sip-tls-port) SIP_TLS_PORT="$2"; shift 2;;
    --bin) BIN="$2"; shift 2;;
    --config) CONFIG="$2"; shift 2;;
    --sounds) DO_SOUNDS=1; shift;;
    --no-sounds) DO_SOUNDS=0; shift;;
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
    # SIP-over-TLS needs the `tls` cargo feature compiled in; keep the default features too.
    FEATURES_ARG=""
    [ "$DO_TLS" = "1" ] && FEATURES_ARG="--features tls"
    log "building commosd (release${FEATURES_ARG:+, $FEATURES_ARG}) — this can take a few minutes…"
    # shellcheck disable=SC2086
    ( cd "$REPO_DIR" && cargo build --release --bin commosd $FEATURES_ARG )
    BIN="$REPO_DIR/target/release/commosd"
  else
    die "commosd binary not found. Provide --bin <path>, put it on PATH, or pass --build (needs cargo)."
  fi
fi
ok "using binary: $BIN"

# ---- create data dir --------------------------------------------------------------------
mkdir -p "$DATA_DIR"
ok "data dir: $DATA_DIR"

# ---- audio prompts (voicemail greeting + retrieval menu) --------------------------------
# CommOS ships no audio of its own. The voicemail "please leave a message after the tone"
# greeting and the *97/*98 retrieval menu use FreePBX's publicly-downloadable prompts, fetched
# here onto this system. They remain the property of FreePBX/Sangoma — we credit them and only
# download (never redistribute). If skipped, voicemail still works with a synthesized beep.
SOUNDS_DIR="$DATA_DIR/sounds"
download_sounds() {
  # Idempotent: if the pack is already present, do nothing (upgrades don't re-download).
  if [ -f "$SOUNDS_DIR/en/vm-intro.ulaw" ] && [ "$FORCE" != "1" ]; then
    ok "audio prompts already installed at $SOUNDS_DIR (skipping download; --force to refresh)"
    return 0
  fi
  command -v curl >/dev/null 2>&1 || { warn "curl not found; skipping audio prompt download (voicemail will use a beep)."; return 0; }
  echo
  log "Audio prompts (voicemail greeting + *97/*98 menu):"
  echo "    These prompts are the work of FreePBX / Sangoma (https://www.freepbx.org/,"
  echo "    https://github.com/FreePBX) and remain their property. CommOS only downloads them"
  echo "    onto this machine — it does not bundle or redistribute them."
  echo "    Source: $SOUNDS_URL"
  # Prompt when interactive and the operator hasn't already decided via a flag.
  if [ -z "$DO_SOUNDS" ] && [ -t 0 ]; then
    printf '    Download the FreePBX audio prompt pack now? [Y/n] '
    read -r _ans || _ans=""
    case "$_ans" in [Nn]*) DO_SOUNDS=0;; *) DO_SOUNDS=1;; esac
  fi
  # Non-interactive with no flag → default to downloading (voicemail is a core feature).
  [ -z "$DO_SOUNDS" ] && DO_SOUNDS=1
  if [ "$DO_SOUNDS" != "1" ]; then
    warn "skipping audio prompt download — voicemail will use a synthesized beep. Re-run with --sounds to add them."
    return 0
  fi
  mkdir -p "$SOUNDS_DIR/en"
  log "downloading FreePBX audio prompts to $SOUNDS_DIR/en …"
  # Non-fatal: a failure (offline install, bad mirror) must not abort the whole install.
  if curl -fsSL "$SOUNDS_URL" | tar -xz -C "$SOUNDS_DIR/en" 2>/dev/null; then
    if [ -f "$SOUNDS_DIR/en/vm-intro.ulaw" ]; then
      ok "audio prompts installed at $SOUNDS_DIR/en (courtesy of FreePBX / Sangoma)"
    else
      warn "audio pack downloaded but vm-intro.ulaw is missing — voicemail will use a beep. Check $SOUNDS_URL layout."
    fi
  else
    warn "could not download audio prompts (offline, or mirror unreachable). Voicemail will use a beep."
    warn "You can add them later: re-run with --sounds, or set SOUNDS_URL to a reachable mirror."
  fi
}
download_sounds

# ---- admin password (as a file-referenced secret, never inline) -------------------------
ADMIN_YAML=""
if [ -n "$ADMIN_PASSWORD" ]; then
  PW_FILE="$DATA_DIR/admin_password"
  printf '%s' "$ADMIN_PASSWORD" > "$PW_FILE"
  chmod 600 "$PW_FILE"
  ADMIN_YAML=$'admin_password:\n  ref_uri: "file://'"$PW_FILE"$'"'
  ok "admin auth enabled (secret at $PW_FILE, referenced from config)"
fi

# ---- SIP-over-TLS (SIPS) certificate ----------------------------------------------------
# Encrypting the SIP signalling channel protects the SDES SRTP keys and call metadata in
# transit. The operator can bring their own cert (e.g. Let's Encrypt) with --tls-cert/--tls-key;
# otherwise, with --tls we generate a self-signed cert so SIPS works out of the box. The private
# key is a file-referenced secret (never inlined — CMOS-14-DEP-083); the cert is a public path.
SIPS_YAML=""
if [ "$DO_TLS" = "1" ]; then
  # Bring-your-own cert (Let's Encrypt / internal CA): use the operator's files as-is.
  if [ -n "$TLS_CERT" ] || [ -n "$TLS_KEY" ]; then
    [ -n "$TLS_CERT" ] && [ -n "$TLS_KEY" ] || die "TLS: --tls-cert and --tls-key must be given together"
    [ -f "$TLS_CERT" ] || die "TLS: cert not found: $TLS_CERT"
    [ -f "$TLS_KEY" ]  || die "TLS: key not found: $TLS_KEY"
    CERT_PATH="$TLS_CERT"
    KEY_PATH="$TLS_KEY"
    ok "SIPS enabled with your certificate: $CERT_PATH"
    warn "ensure the key ($KEY_PATH) is readable by the commosd service user and kept 0600."
  else
    # Self-signed fallback — needs openssl.
    command -v openssl >/dev/null 2>&1 || die "TLS: openssl not found. Install it, or pass --tls-cert/--tls-key."
    TLS_DIR="$DATA_DIR/tls"
    mkdir -p "$TLS_DIR"
    CERT_PATH="$TLS_DIR/sip-cert.pem"
    KEY_PATH="$TLS_DIR/sip-key.pem"
    # SAN covers the media/registrar IP phones connect to (and localhost). 825 days is the max a
    # public CA would issue; fine for a self-signed cert phones are told to trust.
    SAN="subjectAltName=IP:$MEDIA_IP,DNS:localhost,IP:127.0.0.1"
    log "generating a self-signed SIPS certificate (CN=$MEDIA_IP)…"
    if openssl req -x509 -newkey rsa:2048 -nodes \
         -keyout "$KEY_PATH" -out "$CERT_PATH" -days 825 \
         -subj "/CN=$MEDIA_IP" -addext "$SAN" >/dev/null 2>&1; then
      :
    else
      # Older OpenSSL without -addext: fall back to a CN-only cert.
      warn "openssl -addext unsupported; issuing a CN-only cert (no SAN). Upgrade openssl for SAN."
      openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$KEY_PATH" -out "$CERT_PATH" -days 825 -subj "/CN=$MEDIA_IP" >/dev/null 2>&1 \
        || die "TLS: self-signed certificate generation failed"
    fi
    chmod 600 "$KEY_PATH"
    ok "self-signed SIPS certificate: $CERT_PATH (key $KEY_PATH, 0600)"
    warn "self-signed cert: phones must be set to trust it (or disable cert validation on the LAN)."
  fi
  SIPS_YAML="sips_listen: \"0.0.0.0:$SIP_TLS_PORT\""$'\n'"sip_tls_cert: \"$CERT_PATH\""$'\n'"sip_tls_key:"$'\n'"  ref_uri: \"file://$KEY_PATH\""
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
  if [ "$DO_TLS" = "1" ]; then
    echo "# SIP-over-TLS (SIPS): protects SDES SRTP keys + call metadata in transit."
    echo "$SIPS_YAML"
    echo "require_sip_auth: true   # TLS is exposed beyond the LAN; demand per-device SIP credentials."
  fi
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
Documentation=https://github.com/sausin/commos
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$BIN --config $CONFIG
# Run from the data dir so any relative paths resolve against the state it owns.
WorkingDirectory=$DATA_DIR
Restart=on-failure
RestartSec=2
# Each established call holds ~2 UDP relay sockets; the default 1024-fd ceiling caps the box at
# ~500 concurrent calls. Raise it clear of the media plane's real ceiling (cheap insurance).
LimitNOFILE=65536
# Systemd sends SIGTERM; commosd drains gracefully (readiness off, then finishes in-flight).
KillSignal=SIGTERM
TimeoutStopSec=30
# --- logging: commosd logs to stdout/stderr, which systemd captures into the journal. Tag it
# so \`journalctl -t commosd\` / \`-u commosd\` reads cleanly.
SyslogIdentifier=commosd
# --- sandboxing: the daemon only needs to write its own data dir. Lock down the rest so a bug
# can't touch the wider filesystem. ReadWritePaths keeps the SQLite DB, objects, and any config
# rewrite working under a read-only system.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=$DATA_DIR

[Install]
WantedBy=multi-user.target
UNITEOF
  systemctl daemon-reload
  systemctl enable --now commosd.service
  ok "systemd service installed and started"
  log "Logs (journald):   journalctl -u commosd -f"
  log "Service status:    systemctl status commosd"
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
if [ "$DO_TLS" = "1" ]; then
  echo "    • Or over TLS (SIPS): $MEDIA_IP:$SIP_TLS_PORT   (TLS) — encrypts signalling + SRTP keys"
fi
echo "    • Username:           the extension number you assigned in the wizard"
echo "    • (Auto-provision:    set DHCP option 66 to http://$MEDIA_IP:$HTTP_PORT/provision)"
echo
log "Test the call path:"
echo "    • Dial your own number  → echo test (you hear yourself = signalling + audio OK)"
echo "    • Dial another phone's extension → two-way call"
echo "    • Dial *97 → listen to your voicemail (7 delete, 9 save, # next)"
echo
if [ -f "$SOUNDS_DIR/en/vm-intro.ulaw" ]; then
  log "Voicemail prompts: $SOUNDS_DIR/en  (audio courtesy of FreePBX / Sangoma — https://www.freepbx.org/)"
  echo
fi
log "Security posture (secure-by-default — no extra config needed):"
echo "    • The API auto-generates a JWT signing secret at $DATA_DIR/secrets/jwt.key on first boot."
echo "    • From the LAN/loopback, the tenant:<uuid> dev token, dashboard, introspection, and phone"
echo "      auto-provisioning work with zero setup."
echo "    • From a PUBLIC source address, those conveniences are refused automatically: /v1 needs a"
echo "      signed JWT, admin needs an admin session, and provisioning/introspection are LAN-only."
echo "    • SIP digest auth is auto-required for any non-LAN source address (identity is bound to"
echo "      the credential, so one device cannot register or dial as another)."
echo
if [ "$DO_TLS" = "1" ]; then
  ok "SIPS (SIP-over-TLS) is enabled on port $SIP_TLS_PORT."
else
  warn "SIP/RTP signalling is unencrypted over UDP $SIP_PORT. Keep it off the public internet, or"
  warn "re-run with --tls (self-signed) or --tls-cert/--tls-key (e.g. Let's Encrypt) to enable SIPS."
fi
