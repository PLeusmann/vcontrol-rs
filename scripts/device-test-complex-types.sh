#!/usr/bin/env bash
# Device test for writing complex types (Zeitprogramm, Ferienprogramm) on the Vitotronic 200 KO2B.
#
# Runs the vcontrol CLI on the heating Pi over SSH. vcontrol-mqtt holds the Optolink, so it is stopped for
# the test and started again on exit. Every command, its JSON output and the raw telegrams
# (RUST_LOG=vcontrol=trace) go to the log file.
#
#   Part 1  read only: baseline values and raw bytes, incl. the controller's weekday byte in Uhrzeit
#   Part 2  write the Zirkulation Zeitprogramm back unchanged, read it back, compare
#   Part 3  Ferienbeginn to a future date (end stays 1970, so no holiday starts), read back, set back to 1970
#
# Usage: scripts/device-test-complex-types.sh [ssh-host]        (default: heizomat)
#   In a terminal it asks before writing. Without one it only reads, unless WRITE=yes is set.
set -euo pipefail

HOST=${1:-heizomat}
DEVICE=/dev/ttyUSB0
LOG=${LOG:-complex-types-device-test-$(date +%Y%m%d-%H%M%S).log}
ZP=Schaltzeiten_A1M1_ZP   # write target: the circulation pump does the least harm if the encoding is wrong
FUTURE=2027-12-30         # Ferienbeginn for the date write; must be in the future
NO_HOLIDAY=1970-01-01     # what the controller reports when no Ferienprogramm is set

bridge_was_active=no
ferien_dirty=no
tmp=$(mktemp)

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$LOG" >&2; }
fail() { log "FAIL: $*"; exit 1; }

# Asks the person running the test; the answer is logged. Without a terminal the answer is empty.
ask() {
  local answer=
  [ -t 0 ] && read -r -p "$1 " answer
  log "Q: $1 A: ${answer:-<empty>}"
  printf '%s' "$answer"
}

# Runs the CLI on the Pi. JSON output to stdout and the log, trace output to the log only.
vc() {
  log "\$ vcontrol $*"
  local out
  if ! out=$(ssh "$HOST" "RUST_LOG=vcontrol=trace vcontrol -d $DEVICE $(printf '%q ' "$@")" 2>"$tmp"); then
    sed 's/^/    ! /' "$tmp" >>"$LOG"
    fail "vcontrol $* failed: $(tail -n1 "$tmp")"
  fi
  sed 's/^/    | /' "$tmp" >>"$LOG"
  [ -n "$out" ] && printf '%s\n' "$out" | sed 's/^/    /' >>"$LOG"
  printf '%s\n' "$out"
}

# The `value` of a Command as compact JSON.
value() { vc get "$1" | jq -cS .value; }

cleanup() {
  local status=$?
  if [ "$ferien_dirty" = yes ]; then
    log "Restoring FerienBeginnA1M1 to $NO_HOLIDAY"
    # Subshell: `fail` must not exit the trap before the Bridge is started again.
    (vc set FerienBeginnA1M1 "$NO_HOLIDAY" >/dev/null) || log "RESTORE FAILED: set Ferienprogramm start at the controller"
  fi
  if [ "$bridge_was_active" = yes ]; then
    log "Starting vcontrol-mqtt again"
    ssh "$HOST" sudo -n systemctl start vcontrol-mqtt || log "START FAILED: run 'sudo systemctl start vcontrol-mqtt' on $HOST"
    sleep 5
    log "vcontrol-mqtt: $(ssh "$HOST" systemctl is-active vcontrol-mqtt || true)"
  fi
  rm -f "$tmp"
  log "Exit status $status. Log: $LOG"
}
trap cleanup EXIT

# --- Part 0: preconditions ---------------------------------------------------------------------------------

log "Device test complex types on $HOST ($DEVICE)"
log "Local repo: $(git -C "$(dirname "$0")" log -1 --format='%h %s' 2>/dev/null || echo unknown)"
log "On the Pi: $(ssh "$HOST" 'vcontrol --version; dpkg-query -W -f="\${Version}" vcontrol-rs' | tr '\n' ' ')"

if [ "$(ssh "$HOST" systemctl is-active vcontrol-mqtt || true)" = active ]; then
  log "Stopping vcontrol-mqtt (Home Assistant shows the entities as unavailable until the end)"
  ssh "$HOST" sudo -n systemctl stop vcontrol-mqtt
  bridge_was_active=yes
fi

# --- Part 1: read only ---------------------------------------------------------------------------------------

log "Part 1: baseline (read only)"
log "Pi clock: $(ssh "$HOST" 'date "+%Y-%m-%dT%H:%M:%S %A, ISO weekday %u"')"
uhrzeit=$(value Uhrzeit) || uhrzeit=ERROR
log "Uhrzeit = $uhrzeit. Byte 4 of its raw telegram above is the controller's weekday: compare with the Pi's weekday."

hk=$(value Schaltzeiten_A1M1_HK) || hk=ERROR
ww=$(value Schaltzeiten_A1M1_WW) || ww=ERROR
zp=$(value "$ZP") || zp=ERROR
beginn=$(value FerienBeginnA1M1) || beginn=ERROR
ende=$(value FerienEndeA1M1) || ende=ERROR
ferienbetrieb=$(value HK_FerienbetriebA1M1) || ferienbetrieb=ERROR
log "Schaltzeiten_A1M1_HK = $hk"
log "Schaltzeiten_A1M1_WW = $ww"
log "$ZP = $zp"
log "FerienBeginnA1M1 = $beginn, FerienEndeA1M1 = $ende, HK_FerienbetriebA1M1 = $ferienbetrieb"

if [ "$(ask 'Compare the Zirkulation Zeitprogramm with the display now? Matches: y / differs: n / Enter: skip')" = n ]; then
  fail "Zeitprogramm decoding differs from the display, so writing it is not safe"
fi

if [ "${WRITE:-}" != yes ] && [ "$(ask "Part 1 done. Start the write tests (parts 2 and 3)? y/N")" != y ]; then
  log "Stopped before writing (WRITE=yes or answering y runs parts 2 and 3)"
  exit 0
fi

# --- Part 2: Zeitprogramm unchanged write ------------------------------------------------------------------

log "Part 2: write $ZP back unchanged"
jq -e 'type == "object"' <<<"$zp" >/dev/null || fail "$ZP is not a Zeitprogramm object: $zp"

vc set "$ZP" "$zp" >/dev/null
sleep 2
zp_after=$(value "$ZP")
log "$ZP after write = $zp_after"
if [ "$zp_after" != "$zp" ]; then
  log "The Zeitprogramm changed. Restore it at the controller (Zeitprogr. Zirkulation) to: $zp"
  fail "$ZP read back differs from what was written"
fi
log "Part 2 passed: Zeitprogramm written and read back identical"

# --- Part 3: Ferienprogramm date write ---------------------------------------------------------------------

log "Part 3: Ferienprogramm date write"
if [ "$beginn" != "\"$NO_HOLIDAY\"" ] || [ "$ende" != "\"$NO_HOLIDAY\"" ] || [ "$ferienbetrieb" = ERROR ]; then
  log "Skipped: the Ferienprogramm is not in the no-holiday state ($beginn to $ende), and this test does not touch a real one"
  exit 0
fi

ferien_dirty=yes
vc set FerienBeginnA1M1 "$FUTURE" >/dev/null
sleep 2
beginn_after=$(value FerienBeginnA1M1)
ende_after=$(value FerienEndeA1M1)
ferienbetrieb_after=$(value HK_FerienbetriebA1M1)
log "After write: FerienBeginnA1M1 = $beginn_after, FerienEndeA1M1 = $ende_after, HK_FerienbetriebA1M1 = $ferienbetrieb_after"
[ "$beginn_after" = "\"$FUTURE\"" ] || fail "FerienBeginnA1M1 read back $beginn_after, expected \"$FUTURE\""
[ "$ende_after" = "$ende" ] || fail "FerienEndeA1M1 changed to $ende_after"
[ "$ferienbetrieb_after" = "$ferienbetrieb" ] || fail "HK_FerienbetriebA1M1 changed to $ferienbetrieb_after: start in the future with end $NO_HOLIDAY is not inactive"

vc set FerienBeginnA1M1 "$NO_HOLIDAY" >/dev/null
sleep 2
beginn_restored=$(value FerienBeginnA1M1)
log "After restore: FerienBeginnA1M1 = $beginn_restored"
[ "$beginn_restored" = "\"$NO_HOLIDAY\"" ] || fail "FerienBeginnA1M1 read back $beginn_restored after writing $NO_HOLIDAY"
ferien_dirty=no
[ "$(value HK_FerienbetriebA1M1)" = "$ferienbetrieb" ] || fail "HK_FerienbetriebA1M1 changed after the restore"
log "Part 3 passed: date written, read back, and $NO_HOLIDAY written back"
