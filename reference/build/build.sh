#!/usr/bin/env bash
# Parametric cross build for the CommOS single binary.
#
#   build/build.sh            # build the primary target (rpi4 / arm64)
#   build/build.sh rpi4       # build a named target from build/targets.toml
#   build/build.sh amd64
#   build/build.sh all        # build every registered target
#   build/build.sh --list     # list registered targets
#
# Adding an architecture is a row in build/targets.toml + a linker line in
# .cargo/config.toml — no code change (CMOS-14-DEP-042/060).
set -euo pipefail

cd "$(dirname "$0")/.."
REG="build/targets.toml"

# Minimal TOML reader for our fixed shape (id/triple pairs). Pure bash — no tool dependency.
_qval() { sed -E 's/.*"([^"]+)".*/\1/' <<<"$1"; }  # first quoted value on a line

list_targets() {
  while IFS= read -r line; do
    [[ "${line# }" == id\ =* || "${line# }" == id=* ]] && _qval "$line"
  done < "$REG"
}
triple_for() {
  local want="$1" id=""
  while IFS= read -r line; do
    local t="${line#"${line%%[![:space:]]*}"}"  # left-trim
    case "$t" in
      "[[target]]"*) id="" ;;
      id=*|id\ =*)     id="$(_qval "$t")" ;;
      triple=*|triple\ =*) [[ "$id" == "$want" ]] && { _qval "$t"; return; } ;;
    esac
  done < "$REG"
}
primary_target() {
  local id=""
  while IFS= read -r line; do
    local t="${line#"${line%%[![:space:]]*}"}"
    case "$t" in
      "[[target]]"*) id="" ;;
      id=*|id\ =*)   id="$(_qval "$t")" ;;
      primary*=*true*) echo "$id"; return ;;
    esac
  done < "$REG"
}

build_one() {
  local id="$1" triple; triple="$(triple_for "$id")"
  if [[ -z "$triple" ]]; then echo "unknown target '$id' (see --list)" >&2; exit 1; fi
  echo ">> building $id ($triple)"
  rustup target add "$triple" >/dev/null 2>&1 || true
  cargo build --release --target "$triple"
  local out="target/$triple/release/commosd"
  echo "   -> $out"
  file "$out" 2>/dev/null || true
}

case "${1:-}" in
  --list|-l) list_targets ;;
  all)       for t in $(list_targets); do build_one "$t"; done ;;
  "")        build_one "$(primary_target)" ;;
  *)         build_one "$1" ;;
esac
