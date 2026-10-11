#!/usr/bin/env bash
# e2e — the start/stop switch (ticket #22): spec §4.2/§4.3/§4.4/§5 on the
# REAL user manager, on the maintainer's host. Companion to
# e2e-first-vertical.sh (ticket #20), reusing its assertion style.
#
# Scenarios (every timing below is a SECONDS-scale margin — each fixture
# primitive was grounded on the live host before this script was written):
#
#   group sw [a, b, f]:
#     full switch cycle   start a -> start b (slow Type=exec: the CLI's
#                         wall time proves the job was JobRemoved-
#                         confirmed, not fire-and-forget) -> stop b;
#     no-op start         (target active, others inactive -> silent 0);
#     no-op stop          (already inactive -> silent 0);
#     row 6 (job failed)  a active + start f (missing exec) -> exit 6
#                         `job failed` — the switch stopped a FIRST (the
#                         §4.2 order: the target fails after the stop
#                         phase); membership unchanged.
#   group gb [y1, r1, t1] — §4.2 rule, WAIT + RE-ISSUE branch:
#     y1 and r1 are started BEFORE they are members (no group, so the
#     watchdog reaction is vacuous — two active members exist only as
#     this e2e precondition). `uss gb start t1` snapshots both active,
#     stops y1 (ExecStop ~2s — the slow sibling is the timing anchor);
#     y1's ExecStop queues a reload on r1 (ExecReload ~3.5s). The
#     daemon's StopUnit(r1,"fail") then hits a queued reload ->
#     TransactionIsDestructive -> the rule WAITS (reload settles done,
#     r1 is ACTIVE again), RE-ISSUES once (accepted — r1 stops fast),
#     and starts t1: exit 0, silent. Wall >= 4s proves the wait.
#   group gb2 [y2, r2, t2] — rule, WAIT + SETTLED-INACTIVE (satisfied):
#     identical skeleton, but r2's ExecReload replaces a STOP on r2 into
#     systemd mid-reload (~1.1s in; r2's own stop is ~1.7s): the daemon
#     is rejected at StopUnit(r2), its wait sees the reload settle
#     (canceled), re-reads r2 NOT active (the script-owned stop is
#     running), treats the stop phase as satisfied, and starts t2.
#   row 5 before any stop — group nf [m, d]: d active; mask m;
#     `uss nf start m` -> exit 5 `is masked` and d is STILL ACTIVE
#     (the §4.2 loadability check runs BEFORE anything is stopped).
#   row 5 at add — `uss sw add ghost`: exit 5 `not found`, unchanged.
#
# Conflicting-job ABORT (rejected twice -> exit 6 `conflicting job`):
# the second rejection must land in the daemon's ~2-4 ms re-read +
# re-issue window; no script-side job injection can win that race
# deterministically (every queued call settles either before or well
# after it). It is covered deterministically by the fake seam
# (core.rs: rejected_twice_aborts_exit_6_no_start + the remove twin) and
# wire->CLI mapped by tests/cli.rs; here the two reachable branches are
# exercised live. Disclosed in the PR and the ticket resolution.
#
# Host impact: installs/removes fixture units under
# ~/.config/systemd/user, rewrites groups.json, restarts ussd. An EXIT
# trap restores the steady state (no fixtures, no groups, ussd installed
# + enabled + running, empty status).

set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
USS="$REPO/target/debug/uss"
USSD="$REPO/target/debug/ussd"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}"
USSD_UNIT="$CFG/systemd/user/ussd.service"
CFG_USS_DIR="$CFG/uss"
STATE_FILE="$CFG_USS_DIR/groups.json"
RUN_USS="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/uss"
SCRATCH="$(mktemp -d)"

# -- assertions ---------------------------------------------------------------

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
# Runs the real uss, prints the transcript, asserts all three; also
# records wall time in ELAPSED (seconds, integer) for the floor checks.
ELAPSED=0
run_uss() {
  local label="$1" want_rc="$2" want_out="$3" want_err="$4"
  shift 4
  local out err rc t0
  t0=$SECONDS
  out=$("$USS" "$@" </dev/null 2>"$SCRATCH/err"); rc=$?
  ELAPSED=$((SECONDS - t0))
  err="$(cat "$SCRATCH/err")"
  echo "----- $label -----"
  if [ "$#" -gt 0 ]; then echo "\$ uss $*"; else echo "\$ uss"; fi
  [ -n "$out" ] && echo "stdout: $out"
  [ -n "$err" ] && echo "stderr: $err"
  echo "exit:   $rc (elapsed ${ELAPSED}s)"
  echo
  assert_eq "$label: exit code" "$want_rc" "$rc"
  assert_eq "$label: stdout" "$want_out" "$out"
  assert_eq "$label: stderr" "$want_err" "$err"
}

# assert_floor <label> <min-seconds> — the wall time of the last run_uss
# proves a JOB WAIT happened (the switch/§4.2 branches are second-slow by
# construction; a non-waiting path is sub-second).
assert_floor() {
  if [ "$ELAPSED" -ge "$2" ]; then ok "$1 (>=${2}s, was ${ELAPSED}s)"; else bad "$1 (>=${2}s, was ${ELAPSED}s)"; fi
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
# cargo test alone does not re-link the plain bin executables — refuse a
# stale build (the ticket #20 lesson; the #22 run tests the switch code).
for b in "$USS" "$USSD"; do
  NEWEST_SRC="$(find "$REPO/src" "$REPO/Cargo.toml" "$REPO/Cargo.lock" -newer "$b" -print -quit 2>/dev/null)"
  [ -n "$NEWEST_SRC" ] && { echo "FATAL: $b is stale ($NEWEST_SRC is newer) — run cargo build" >&2; exit 2; }
done

SLEEP_BIN=""
for c in /run/current-system/sw/bin/sleep /bin/sleep /usr/bin/sleep; do
  [ -x "$c" ] && SLEEP_BIN="$c" && break
done
[ -n "$SLEEP_BIN" ] || SLEEP_BIN="$(command -v sleep)"
TRUE_BIN=""
for c in /run/current-system/sw/bin/true /bin/true /usr/bin/true; do
  [ -x "$c" ] && TRUE_BIN="$c" && break
done
[ -n "$TRUE_BIN" ] || TRUE_BIN="$(command -v true)"
SYSCTL=(systemctl --user)

UNIT_DIR="$CFG/systemd/user"
# The fixture set (names prefixed uss-e2e-).
FIXTURES=(a b f y1 r1 t1 y2 r2 t2 m d)

fixture_file() { printf '%s\n' "$UNIT_DIR/uss-e2e-$1.service"; }

# -- fixture unit bodies --------------------------------------------------------

# Install one fixture unit file.
write_fixture() {
  local name="$1" content="$2"
  printf '%s' "$content" >"$(fixture_file "$name")"
}

install_fixtures() {
  # sw group: plain + slow-confirms-start + missing-exec.
  write_fixture a "[Unit]
Description=uss e2e switch member A

[Service]
ExecStart=$SLEEP_BIN 300
"
  # Type=exec: the start job settles only when the main process is exec'd
  # (after the 2s ExecStartPre) — a >=2s CLI wall time proves the switch
  # waited for JobRemoved rather than fire-and-forget.
  write_fixture b "[Unit]
Description=uss e2e switch member B (slow start)

[Service]
Type=exec
ExecStartPre=$SLEEP_BIN 2
ExecStart=$SLEEP_BIN 300
"
  # Type=oneshot: the START job settles only when the process exits 0;
  # exit 9 makes the job itself fail (result `failed` -> row 6).
  # (Type=simple with a missing exec is NOT a job failure: the start job
  # completes at fork, the exec error lands after it — host-grounded.)
  write_fixture f "[Unit]
Description=uss e2e switch member F (fails its start job)

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'exit 9'
"
  # gb: y1's stop is the timing anchor; its ExecStop queues a reload on
  # r1 (~1.1s into y1's ~2s stop; r1's reload takes ~3.5s) so the
  # daemon's StopUnit(r1) lands squarely on the queued reload.
  write_fixture y1 "[Unit]
Description=uss e2e conflict anchor Y1

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$TRUE_BIN
ExecStop=/bin/sh -c '$SLEEP_BIN 1.1; systemctl --user reload uss-e2e-r1 & $SLEEP_BIN 0.4'
"
  write_fixture r1 "[Unit]
Description=uss e2e conflict member R1

[Service]
Type=simple
ExecStart=$SLEEP_BIN 300
ExecReload=$SLEEP_BIN 3.5
"
  write_fixture t1 "[Unit]
Description=uss e2e switch target T1

[Service]
ExecStart=$SLEEP_BIN 300
"
  # gb2: same skeleton; r2's reload queues a REPLACE-mode stop of r2
  # (~1.1s in) which cancels the reload and keeps a stop job pending
  # ~1.7s — so the daemon is rejected, waits (the reload settles
  # canceled), re-reads r2 NOT active, and starts t2 without touching r2.
  write_fixture y2 "[Unit]
Description=uss e2e conflict anchor Y2

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$TRUE_BIN
ExecStop=/bin/sh -c '$SLEEP_BIN 1.1; systemctl --user reload uss-e2e-r2 & $SLEEP_BIN 0.4'
"
  write_fixture r2 "[Unit]
Description=uss e2e conflict member R2

[Service]
Type=simple
ExecStart=$SLEEP_BIN 300
ExecReload=/bin/sh -c '$SLEEP_BIN 1.1; systemctl --user stop --job-mode=replace uss-e2e-r2 & $SLEEP_BIN 0.9'
"
  write_fixture t2 "[Unit]
Description=uss e2e switch target T2

[Service]
ExecStart=$SLEEP_BIN 300
"
  # nf: masked-target scenario.
  write_fixture m "[Unit]
Description=uss e2e masked member M

[Service]
ExecStart=$SLEEP_BIN 300
"
  write_fixture d "[Unit]
Description=uss e2e bystander D (must survive the masked-target abort)

[Service]
ExecStart=$SLEEP_BIN 300
"
  "${SYSCTL[@]}" daemon-reload
}

remove_fixtures() {
  local n
  for n in "${FIXTURES[@]}"; do
    "${SYSCTL[@]}" stop "uss-e2e-$n.service" 2>/dev/null || true
    rm -f "$(fixture_file "$n")"
  done
  rm -f "$UNIT_DIR/uss-e2e-m.service" # mask symlink (if any)
  "${SYSCTL[@]}" reset-failed "${FIXTURES[@]/#/uss-e2e-}" 2>/dev/null || true
  "${SYSCTL[@]}" daemon-reload
}

# -- teardown (always runs; restores the steady state) ------------------------

teardown() {
  echo "== teardown =="
  # Members first (plain stops, not switches), then the files.
  local n
  for n in "${FIXTURES[@]}"; do
    "${SYSCTL[@]}" stop "uss-e2e-$n.service" 2>/dev/null || true
  done
  "${SYSCTL[@]}" unmask uss-e2e-m.service 2>/dev/null || true
  remove_fixtures
  # ussd holds the state in memory — stop it, wipe state, let the next
  # uss re-bootstrap a clean daemon (the #20 pattern).
  "${SYSCTL[@]}" stop ussd.service 2>/dev/null || true
  rm -rf "$CFG_USS_DIR" "$RUN_USS"
  out="$("$USS" </dev/null 2>&1)" || true
  [ -z "$out" ] || echo "NOTE: final status not empty: $out"
  assert_eq "teardown: ussd active (steady state)" "active" "$(unit_is ussd.service)"
  check_state "teardown" '<absent>'
  echo "== summary: $PASS ok, $FAIL failed =="
  rm -rf "$SCRATCH"
  # The trap's last command does NOT set the script status —
  # an explicit exit is required, or a run with FAILs exits 0 and a
  # gate built on `bash scripts/…` green-lights silently.
  if [ "$FAIL" -eq 0 ]; then exit 0; else exit 1; fi
}
trap 'teardown' EXIT

# -- clean slate ---------------------------------------------------------------

echo "== e2e: start/stop switch vertical (ticket #22) =="
echo "host:   $(id -un)@$(hostname)"
echo "uss:    $USS"
echo "sleep:  $SLEEP_BIN"
echo
echo "== clean slate =="
# Leftovers from an interrupted run: stop, unmask, drop files and state.
for n in "${FIXTURES[@]}"; do
  "${SYSCTL[@]}" stop "uss-e2e-$n.service" 2>/dev/null || true
done
"${SYSCTL[@]}" unmask uss-e2e-m.service 2>/dev/null || true
remove_fixtures
"${SYSCTL[@]}" stop ussd.service 2>/dev/null || true
rm -rf "$CFG_USS_DIR" "$RUN_USS"
assert_ok "ussd stopped for the vertical" test "$(unit_is ussd.service)" != "active"

install_fixtures
echo "installed ${#FIXTURES[@]} fixture units"
echo

# Loadability is read on demand (spec §10 erratum); the ghost probe
# grounds the not-found-as-data machinery the add path relies on.
assert_eq "ghost not-found on host" "not-found" \
  "$(unit_loadstate uss-e2e-ghost-nothing.service)"
echo

# -- pre-arm: two active members via out-of-group starts ----------------------
#
# Exclusivity is enforced on the START paths (switch + watchdog); `add`
# never changes the active state (§4.2) — so the precondition "group has
# two active members" (which the §4.2 rule needs) is only constructible
# by starting the units while they are not members of any group, then
# adding them. ussd is running (first uss bootstraps it) BEFORE the
# starts so its watchdog sees them — vacuously (no group owns them yet).
echo "== pre-arm =="
run_uss "pre-arm: bootstrap ussd" 0 "" ""
assert_eq "pre-arm: ussd active" "active" "$(unit_is ussd.service)"
"${SYSCTL[@]}" start uss-e2e-y1 uss-e2e-r1 uss-e2e-y2 uss-e2e-r2
for n in y1 r1 y2 r2; do
  assert_eq "pre-arm: uss-e2e-$n active" "active" "$(unit_is "uss-e2e-$n.service")"
done
echo

# -- the vertical ---------------------------------------------------------------

# 1. Adds (group create lifecycle + the §4.3 add-order arrays).
run_uss "1a: add a" 0 "" "" sw add uss-e2e-a
run_uss "1b: add b" 0 "" "" sw add uss-e2e-b
run_uss "1c: add f" 0 "" "" sw add uss-e2e-f
S_1='{"groups":{"sw":["uss-e2e-a.service","uss-e2e-b.service","uss-e2e-f.service"]},"version":1}'
check_state "1" "$S_1"

run_uss "1d: gb add y1" 0 "" "" gb add uss-e2e-y1
run_uss "1e: gb add r1" 0 "" "" gb add uss-e2e-r1
run_uss "1f: gb add t1" 0 "" "" gb add uss-e2e-t1
run_uss "1g: gb2 add y2" 0 "" "" gb2 add uss-e2e-y2
run_uss "1h: gb2 add r2" 0 "" "" gb2 add uss-e2e-r2
run_uss "1i: gb2 add t2" 0 "" "" gb2 add uss-e2e-t2
run_uss "1j: nf add m" 0 "" "" nf add uss-e2e-m
run_uss "1k: nf add d" 0 "" "" nf add uss-e2e-d
S_2='{"groups":{"gb":["uss-e2e-y1.service","uss-e2e-r1.service","uss-e2e-t1.service"],"gb2":["uss-e2e-y2.service","uss-e2e-r2.service","uss-e2e-t2.service"],"nf":["uss-e2e-m.service","uss-e2e-d.service"],"sw":["uss-e2e-a.service","uss-e2e-b.service","uss-e2e-f.service"]},"version":1}'
check_state "1" "$S_2"
show_state

# 2. Status accuracy with the pre-armed actives: markers exactly where
#    the live ActiveState says so (§4.3 snapshot semantics).
OUT_2=$'gb\n  uss-e2e-y1.service - Active\n  uss-e2e-r1.service - Active\n  uss-e2e-t1.service\ngb2\n  uss-e2e-y2.service - Active\n  uss-e2e-r2.service - Active\n  uss-e2e-t2.service\nnf\n  uss-e2e-m.service\n  uss-e2e-d.service\nsw\n  uss-e2e-a.service\n  uss-e2e-b.service\n  uss-e2e-f.service'
run_uss "2: status (pre-armed markers)" 0 "$OUT_2" ""

# 3. Full switch cycle on sw: start a; switch to b (a stops, b's slow
#    Type=exec start job confirms); stop b.
run_uss "3a: start a" 0 "" "" sw start uss-e2e-a
assert_eq "3a: a active (live)" "active" "$(unit_is uss-e2e-a.service)"
run_uss "3b: status sw (a active)" 0 $'sw\n  uss-e2e-a.service - Active\n  uss-e2e-b.service\n  uss-e2e-f.service' "" sw
run_uss "3c: switch a -> b" 0 "" "" sw start uss-e2e-b
assert_floor "3c: switch waited for b's start job" 2
assert_eq "3c: a stopped (live)" "inactive" "$(unit_is uss-e2e-a.service)"
assert_eq "3c: b active (live)" "active" "$(unit_is uss-e2e-b.service)"
run_uss "3d: status sw (only b active)" 0 $'sw\n  uss-e2e-a.service\n  uss-e2e-b.service - Active\n  uss-e2e-f.service' "" sw
check_state "3" "$S_2"   # the switch never touches state
run_uss "3e: stop b" 0 "" "" sw stop uss-e2e-b
assert_eq "3e: b inactive (live)" "inactive" "$(unit_is uss-e2e-b.service)"

# 4. No-op cases (§4.2): both silent exit 0.
run_uss "4a: no-op stop (already inactive)" 0 "" "" sw stop uss-e2e-a
assert_eq "4a: a still inactive" "inactive" "$(unit_is uss-e2e-a.service)"
run_uss "4b: start a" 0 "" "" sw start uss-e2e-a
run_uss "4c: no-op start (target active, others inactive)" 0 "" "" sw start uss-e2e-a
assert_eq "4c: a still active" "active" "$(unit_is uss-e2e-a.service)"
run_uss "4d: stop a (leave sw all-inactive)" 0 "" "" sw stop uss-e2e-a

# 5. §4.2 conflicting-job, WAIT + RE-ISSUE (group gb):
#    StopUnit(r1) rejected by y1's queued reload -> wait -> r1 ACTIVE ->
#    re-issue once -> done. Wall floor proves the wait.
run_uss "5: gb switch (conflict wait + re-issue)" 0 "" "" gb start uss-e2e-t1
assert_floor "5: reloaded member settled then re-stopped" 4
assert_eq "5: y1 stopped" "inactive" "$(unit_is uss-e2e-y1.service)"
assert_eq "5: r1 stopped (via the re-issue)" "inactive" "$(unit_is uss-e2e-r1.service)"
assert_eq "5: t1 active" "active" "$(unit_is uss-e2e-t1.service)"
run_uss "5: status gb" 0 $'gb\n  uss-e2e-y1.service\n  uss-e2e-r1.service\n  uss-e2e-t1.service - Active' "" gb
check_state "5" "$S_2"   # state untouched through the whole conflict dance
run_uss "5z: stop t1" 0 "" "" gb stop uss-e2e-t1

# 6. §4.2 conflicting-job, WAIT + SETTLED-INACTIVE (group gb2):
#    the replace-stop bomb cancels the reload; the daemon's re-read sees
#    r2 not-active -> stop phase satisfied -> t2 starts, r2 never
#    touched by the daemon.
run_uss "6: gb2 switch (conflict settles inactive)" 0 "" "" gb2 start uss-e2e-t2
assert_floor "6: waited on the canceled reload + script-owned stop" 2
assert_eq "6: y2 stopped" "inactive" "$(unit_is uss-e2e-y2.service)"
# Deterministic: the script's replace-stop lands while r2's ExecReload
# control process is still running; the stop's SIGTERM kills it, the
# unit's result is 'signal' -> ActiveState failed. Not-active is what
# the §4.2 rule reads (the stop phase is satisfied either way).
assert_eq "6: r2 not active (failed: stop raced the reload control)" "failed" "$(unit_is uss-e2e-r2.service)"
assert_eq "6: t2 active" "active" "$(unit_is uss-e2e-t2.service)"
check_state "6" "$S_2"
run_uss "6z: stop t2" 0 "" "" gb2 stop uss-e2e-t2

# 7. Row 6 via a start that FAILS its job (missing exec), group sw:
#    the switch stops the active member first (§4.2 order), the target's
#    start job fails -> exit 6, target not active; membership unchanged.
run_uss "7a: start a" 0 "" "" sw start uss-e2e-a
run_uss "7b: start f (job fails)" 6 "" "uss: failed to start uss-e2e-f.service: job failed" sw start uss-e2e-f
assert_eq "7b: f failed (live ActiveState)" "failed" "$(unit_is uss-e2e-f.service)"
assert_eq "7b: a was stopped by the switch first (§4.2 order)" "inactive" "$(unit_is uss-e2e-a.service)"
check_state "7" "$S_2"   # membership unchanged by the failed switch
run_uss "7c: status sw (f failed prints bare)" 0 $'sw\n  uss-e2e-a.service\n  uss-e2e-b.service\n  uss-e2e-f.service' "" sw

# 8. Row 5 (masked) at start — BEFORE any stop: d stays active (§4.2:
#    the loadability check precedes the stop phase; the ticket's
#    "prior member still active — verify" clause, which holds exactly
#    for the loadability-abort class).
run_uss "8a: start d" 0 "" "" nf start uss-e2e-d
# Host-grounded: `systemctl --user mask` REFUSES when the unit file
# already exists in the user config dir ("File already exists") — the
# canonical masked shape is a /dev/null symlink, so the file goes first.
rm -f "$UNIT_DIR/uss-e2e-m.service"
"${SYSCTL[@]}" mask uss-e2e-m.service >/dev/null 2>&1
# mask's internal daemon-reload settles asynchronously — wait for the
# LoadState to actually read masked (bounded).
for i in 1 2 3 4 5 6 7 8 9 10; do
  [ "$(unit_loadstate uss-e2e-m.service)" == "masked" ] && break
  sleep 0.25
done
assert_eq "8: m masked on host" "masked" "$(unit_loadstate uss-e2e-m.service)"
# mask auto-reloads; the daemon re-validates member files (read-only,
# membership untouched — watchdog.rs unit-test semantics) — give the
# pump a beat to process the UnitFilesChanged signal.
sleep 0.5
run_uss "8b: start m (masked)" 5 "" "uss: service uss-e2e-m.service is masked" nf start uss-e2e-m
assert_eq "8b: d STILL ACTIVE (abort happened before any stop)" "active" "$(unit_is uss-e2e-d.service)"
assert_eq "8b: m not active" "inactive" "$(unit_is uss-e2e-m.service)"
check_state "8" "$S_2"
"${SYSCTL[@]}" unmask uss-e2e-m.service >/dev/null 2>&1
run_uss "8c: stop d" 0 "" "" nf stop uss-e2e-d

# 9. Row 5 (not-found) at add — client normalization + daemon check:
#    exit 5, no state.
run_uss "9: add ghost" 5 "" "uss: service uss-e2e-ghost.service not found" sw add uss-e2e-ghost
check_state "9" "$S_2"

# 10. Final full status — everything inactive, byte-exact §4.3 layout.
run_uss "10: status (all groups, none active)" 0 $'gb\n  uss-e2e-y1.service\n  uss-e2e-r1.service\n  uss-e2e-t1.service\ngb2\n  uss-e2e-y2.service\n  uss-e2e-r2.service\n  uss-e2e-t2.service\nnf\n  uss-e2e-m.service\n  uss-e2e-d.service\nsw\n  uss-e2e-a.service\n  uss-e2e-b.service\n  uss-e2e-f.service' ""

# 11. Group-lifecycle spot check — remove empties a group (§5) while the
#     daemon runs (state file persists with the remaining groups).
run_uss "11a: nf remove m" 0 "" "" nf remove uss-e2e-m
run_uss "11b: nf remove d" 0 "" "" nf remove uss-e2e-d
S_FINAL='{"groups":{"gb":["uss-e2e-y1.service","uss-e2e-r1.service","uss-e2e-t1.service"],"gb2":["uss-e2e-y2.service","uss-e2e-r2.service","uss-e2e-t2.service"],"sw":["uss-e2e-a.service","uss-e2e-b.service","uss-e2e-f.service"]},"version":1}'
check_state "11" "$S_FINAL"
run_uss "11c: status (no nf group)" 0 $'gb\n  uss-e2e-y1.service\n  uss-e2e-r1.service\n  uss-e2e-t1.service\ngb2\n  uss-e2e-y2.service\n  uss-e2e-r2.service\n  uss-e2e-t2.service\nsw\n  uss-e2e-a.service\n  uss-e2e-b.service\n  uss-e2e-f.service' ""

echo
echo "== all scenarios done =="
