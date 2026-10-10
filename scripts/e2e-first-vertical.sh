#!/usr/bin/env bash
# e2e — first vertical (ticket #20): spec §4.3/§4.4/§5/§8 on the REAL
# user manager, on the maintainer's host.
#
#   clean slate -> uss (bootstrap installs + starts ussd)
#   -> uss dev add sleep-fixture   -> uss (group, no marker — inactive)
#   -> uss dev start sleep-fixture -> uss ( - Active)
#   -> uss dev stop  sleep-fixture -> uss (marker gone)
#   -> uss dev remove sleep-fixture-> uss (no output, exit 0)
#
# Every step asserts the §4.4 exit code, the §4.3 stdout (byte-exact),
# and empty stderr on success; groups.json is asserted per §5 (schema
# v1, membership only, add order, full *.service names) after each
# step that touches state.
#
# Host impact: wipes the ussd unit + $XDG_CONFIG_HOME/uss (bootstrap
# reinstalls both) and installs a trivial sleep-fixture user unit. An
# EXIT trap restores the steady state (ussd installed + enabled +
# running, fixture gone, no groups) even on assertion failure.
#
# Requires: a real user session (user manager running), `cargo build`
# (target/debug/{uss,ussd}), and `jq` + `systemctl` on PATH.

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
USS="$REPO/target/debug/uss"
USSD="$REPO/target/debug/ussd"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}"
USSD_UNIT="$CFG/systemd/user/ussd.service"
USSD_WANTS="$CFG/systemd/user/default.target.wants/ussd.service"
FIXTURE_UNIT="$CFG/systemd/user/sleep-fixture.service"
CFG_USS_DIR="$CFG/uss"
STATE_FILE="$CFG_USS_DIR/groups.json"
RUN_USS="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/uss"
SCRATCH="$(mktemp -d)"

# -- assertions -------------------------------------------------------------

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# assert_eq <label> <want> <got>
assert_eq() {
  if [ "$2" == "$3" ]; then ok "$1"; else bad "$1 — want [$2] got [$3]"; fi
}

# assert_ok <label> <cmd...> — assert the command exits 0.
assert_ok() {
  local label="$1"
  shift
  if "$@" >/dev/null 2>&1; then ok "$label"; else bad "$label"; fi
}

# run_uss <label> <want-rc> <want-stdout> <want-stderr> <args...>
# Runs the real uss, prints the transcript, asserts all three.
run_uss() {
  local label="$1" want_rc="$2" want_out="$3" want_err="$4"
  shift 4
  local out err rc
  out=$("$USS" "$@" </dev/null 2>"$SCRATCH/err"); rc=$?
  err="$(cat "$SCRATCH/err")"
  echo "----- $label -----"
  if [ "$#" -gt 0 ]; then echo "\$ uss $*"; else echo "\$ uss"; fi
  [ -n "$out" ] && echo "stdout: $out"
  [ -n "$err" ] && echo "stderr: $err"
  echo "exit:   $rc"
  echo
  assert_eq "$label: exit code" "$want_rc" "$rc"
  assert_eq "$label: stdout" "$want_out" "$out"
  assert_eq "$label: stderr" "$want_err" "$err"
}

# check_state <label> <want> — groups.json as `jq -S -c`, or <absent>.
check_state() {
  local got
  if [ -e "$STATE_FILE" ]; then got="$(jq -S -c . "$STATE_FILE" 2>/dev/null)"; else got='<absent>'; fi
  assert_eq "$1: groups.json" "$2" "$got"
}

show_state() {
  if [ -e "$STATE_FILE" ]; then
    echo "  groups.json:"
    sed 's/^/    /' "$STATE_FILE"
    echo
  else
    echo "  groups.json: <absent>"
  fi
}

unit_is() { systemctl --user is-active "$1" 2>/dev/null; }
unit_loadstate() { systemctl --user show -p LoadState --value "$1" 2>/dev/null; }

# -- preflight ----------------------------------------------------------------

for bin in jq systemctl sha256sum; do
  command -v "$bin" >/dev/null || { echo "FATAL: $bin not on PATH" >&2; exit 2; }
done
[ -x "$USS" ] && [ -x "$USSD" ] || { echo "FATAL: $USS / $USSD missing — run cargo build first" >&2; exit 2; }
# cargo test alone does not re-link the plain bin executables — refuse to
# run a stale build (it would "test" the wrong code, ticket #20 lesson).
NEWEST_SRC="$(find "$REPO/src" "$REPO/Cargo.toml" -newer "$USS" -print -quit 2>/dev/null)"
[ -n "$NEWEST_SRC" ] && { echo "FATAL: $USS is stale ($NEWEST_SRC is newer) — run cargo build" >&2; exit 2; }
USSD_ABS="$(readlink -f "$USSD")"

SLEEP_BIN=""
for c in /run/current-system/sw/bin/sleep /bin/sleep /usr/bin/sleep; do
  [ -x "$c" ] && SLEEP_BIN="$c" && break
done
[ -n "$SLEEP_BIN" ] || SLEEP_BIN="$(command -v sleep)"

# What bootstrap MUST write (spec §9 template + the resolved ussd path).
printf '[Unit]\nDescription=uss — user service switcher daemon\nDocumentation=https://github.com/kido5217/user-service-switcher/blob/main/docs/spec/uss-ussd.md\n\n[Service]\nExecStart=%s\nRestart=on-failure\nRestartSec=1s\n\n[Install]\nWantedBy=default.target\n' \
  "$USSD_ABS" >"$SCRATCH/unit.expected"

# §5 expectations, in jq -S -c form.
STATE_AFTER_ADD='{"groups":{"dev":["sleep-fixture.service"]},"version":1}'
STATE_EMPTY='{"groups":{},"version":1}'

# §4.3 expectations.
OUT_DEV_INACTIVE=$'dev\n  sleep-fixture.service'
OUT_DEV_ACTIVE=$'dev\n  sleep-fixture.service - Active'

# -- teardown (always runs; restores the steady state) ------------------------

teardown() {
  echo "== teardown =="
  if [ -e "$FIXTURE_UNIT" ]; then
    systemctl --user stop sleep-fixture 2>/dev/null || true
    rm -f "$FIXTURE_UNIT"
    systemctl --user daemon-reload 2>/dev/null || true
    echo "fixture unit removed"
  fi
  # Drop the e2e group (stops the member first if it is still active);
  # the following bare `uss` also restores ussd if a failure happened
  # before bootstrap had run.
  "$USS" dev remove sleep-fixture </dev/null >/dev/null 2>&1 || true
  local out
  out=$("$USS" </dev/null 2>&1) || true
  [ -z "$out" ] || echo "NOTE: final status not empty: $out"
  echo "== summary: $PASS ok, $FAIL failed =="
  rm -rf "$SCRATCH"
  [ "$FAIL" -eq 0 ]
}
trap 'teardown' EXIT

# -- clean slate ---------------------------------------------------------------

echo "== e2e: first vertical (ticket #20) =="
echo "host:   $(id -un)@$(hostname)"
echo "uss:    $USS"
echo "ussd:   $USSD_ABS"
echo "sleep:  $SLEEP_BIN"
echo
echo "== clean slate =="
# Leftover fixture from an interrupted run: stop it and drop the unit.
if [ "$(unit_is sleep-fixture.service)" == "active" ]; then
  systemctl --user stop sleep-fixture || true
fi
rm -f "$FIXTURE_UNIT"
systemctl --user stop ussd.service 2>/dev/null || true
rm -f "$USSD_UNIT" "$USSD_WANTS"
rm -rf "$CFG_USS_DIR" "$RUN_USS"
systemctl --user daemon-reload
assert_eq "ussd daemon stopped" "inactive" "$(unit_is ussd.service)"
assert_ok "ussd unit file absent" test ! -e "$USSD_UNIT"
assert_ok "state dir absent" test ! -e "$CFG_USS_DIR"
echo

# -- fixture unit ---------------------------------------------------------------

echo "== fixture unit =="
cat >"$FIXTURE_UNIT" <<EOF
[Unit]
Description=uss e2e fixture — trivial sleep service

[Service]
ExecStart=$SLEEP_BIN 300
EOF
systemctl --user daemon-reload
echo "installed $FIXTURE_UNIT:"
sed 's/^/  /' "$FIXTURE_UNIT"
echo
# Loadability is read on demand from the unit object (spec §10, as the
# daemon does) — a freshly installed unit reads `loaded` here even before
# anything else has touched it; no reload race to wait out.
assert_eq "fixture loaded" "loaded" "$(unit_loadstate sleep-fixture.service)"
assert_eq "fixture inactive before start" "inactive" "$(unit_is sleep-fixture.service)"

# -- the vertical -----------------------------------------------------------------

# 1. First use: bootstrap installs + starts ussd; no groups → no output.
run_uss "step 1: first use (bootstrap)" 0 "" ""
assert_ok "step 1: unit file == §9 template" cmp -s "$USSD_UNIT" "$SCRATCH/unit.expected"
assert_eq "step 1: ussd active" "active" "$(unit_is ussd.service)"
assert_eq "step 1: ussd enabled" "enabled" "$(systemctl --user is-enabled ussd.service 2>/dev/null)"
check_state "step 1" '<absent>'
show_state
UNIT_HASH_1="$(sha256sum "$USSD_UNIT" | cut -d' ' -f1)"
echo

# 2. add — normalize + loadability check + persist.
run_uss "step 2: add" 0 "" "" dev add sleep-fixture
check_state "step 2" "$STATE_AFTER_ADD"
show_state

# 3. status — the group prints; no marker while the member is inactive.
run_uss "step 3: status (group, member inactive)" 0 "$OUT_DEV_INACTIVE" ""
assert_eq "step 3: fixture inactive (live)" "inactive" "$(unit_is sleep-fixture.service)"
show_state

# 4. start — the switch (nothing else active); state untouched by runtime.
run_uss "step 4: start" 0 "" "" dev start sleep-fixture
check_state "step 4" "$STATE_AFTER_ADD"
show_state

# 5. status — the running member gets the marker.
run_uss "step 5: status ( - Active)" 0 "$OUT_DEV_ACTIVE" ""
assert_eq "step 5: fixture active (live)" "active" "$(unit_is sleep-fixture.service)"
show_state

# 6. stop.
run_uss "step 6: stop" 0 "" "" dev stop sleep-fixture
check_state "step 6" "$STATE_AFTER_ADD"
show_state

# 7. status — marker gone.
run_uss "step 7: status (marker gone)" 0 "$OUT_DEV_INACTIVE" ""
show_state

# 8. remove — last member: the group key is deleted, the file persists.
run_uss "step 8: remove" 0 "" "" dev remove sleep-fixture
check_state "step 8" "$STATE_EMPTY"
show_state

# 9. status — no groups → no output, exit 0; bootstrap stayed idempotent
#    (the §8 step-3 byte-compare found the unit current — no rewrite).
run_uss "step 9: status (no groups — no output)" 0 "" ""
assert_eq "step 9: unit file unchanged (no rewrite)" "$UNIT_HASH_1" \
  "$(sha256sum "$USSD_UNIT" | cut -d' ' -f1)"
