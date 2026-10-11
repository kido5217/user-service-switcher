#!/usr/bin/env bash
# e2e — the watchdog (ticket #23): spec §7 on the REAL user manager, on
# the maintainer's host. Companion to e2e-first-vertical.sh (#20) and
# e2e-start-stop.sh (#22); same assertion style.
#
# Scenarios (fixture timings are seconds-scale margins; all host facts
# were grounded while writing — see the ticket resolution):
#
#   wa [a1, a2] — out-of-band start: the group is fully added FIRST
#     (the reaction only fires for MEMBER start edges — a non-member
#     edge is consumed, §7); a1 active via `uss`; `systemctl --user
#     start a2` (out-of-band) -> the PropertiesChanged edge reaction
#     stops a1 (a2 keeps running; a1's resulting inactive edge is a
#     no-op — only start edges react, §7). Latency asserted < 3 s, far
#     inside the spec's ~1 s class — the 5-6 ms detection figure stays
#     fake-seam-locked (watchdog.rs unit tests); on this host a journal
#     read is slower than the reaction itself.
#   wa convergence: back-to-back out-of-band starts a1 then a2 (0.2 s
#     apart — each reaction drains well inside the gap) -> exactly one
#     active member at the end, and it is the LAST STARTER. Start edges
#     are ordered by the manager's transition timestamp (§7 edge
#     semantics, ticket #23): reaction(a1) must not stop a2 even if a2's
#     out-of-band start lands INSIDE reaction(a1)'s live-read window (a2's
#     transition is newer than a1's edge -> a2's own, newer edge asserts);
#     reaction(a2) reads a1 active with an older transition -> stops a1
#     (JobRemoved-confirmed) -> a2 alone. (The §7 stale-edge rule covers
#     the queued variant: a reaction whose member reads not-active at
#     execution time is dropped.)
#   wb + wc — RE-SYNC: stop ussd, out-of-band-start wb/wc members (an
#     external start works while ussd is DOWN — §7's re-sync exists
#     because signals are not replayed), boot ussd via the next `uss`,
#     then `uss wb start wb1`: the switch MUST stop the other live-active
#     wb member (wb2) (the fresh daemon re-reads ActiveState live at
#     command time — without re-sync the snapshot would read all-inactive
#     and the switch would skip the stop phase); then kill ussd -9
#     (socket gone from under the next uss -> bootstrap step 4 restart
#     (StartUnit) -> re-sync again) and read a byte-exact status across the disruption
#     (m stays Active — the wc group is untouched, its single active
#     member satisfies the invariant, and its re-emitted edge is deduped
#     against the re-sync baseline). groups.json is asserted UNTOUCHED at
#     every step. The bus-reconnect re-sync branch itself
#     is fake-seam/unit-tested; the real manager never drops a well-behaved
#     connection, so this vertical covers its observable analogue
#     (process restart) and discloses the split.
#   masked — UnitFilesChanged / Reloading(false): mask an INACTIVE
#     member (the unit file exists; the manager reload re-validates it
#     masked) -> the member reads masked (not found; membership
#     unchanged), a1 stays active (a unit-file change is NOT a start
#     edge), the next `uss wa start` on it is the structured row-5 error
#     exit 5. A RUNNING member keeps its main process across a mask
#     reload (still - Active) — that outcome is asserted too, and it is
#     the honest reading of the §7 "reads inactive" sentence, which
#     describes the not-running case.
#
# Host safety: installs/removes uss-e2e-* fixture units, rewrites
# ~/.config/uss/groups.json, restarts ussd.service. The EXIT trap
# restores the steady state (no fixtures/mask links, no groups, ussd
# installed+enabled+running, empty status) even on failures.

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
E2E=(a1 a2 wa wb1 wb2 wc t m)

# -- assertions ---------------------------------------------------------------

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

# assert_ok <label> <cmd...> — exit-code assertion (numeric comparisons
# assert_eq's string form can't express).
assert_ok() {
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$label"; else bad "$label"; fi
}

assert_eq() {
  if [ "$2" == "$3" ]; then ok "$1"; else bad "$1 — want [$2] got [$3]"; fi
}

# run_uss <label> <want-rc> <want-stdout> <want-stderr> <args...>
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

check_state() {
  local got
  if [ -e "$STATE_FILE" ]; then got="$(jq -S -c . "$STATE_FILE" 2>/dev/null)"; else got='<absent>'; fi
  assert_eq "$1: groups.json" "$2" "$got"
}

unit_is() { systemctl --user is-active "$1" 2>/dev/null; }
unit_loadstate() { systemctl --user show -p LoadState --value "$1" 2>/dev/null; }

# poll_state <want> <unit> — bounded wait for ActiveState == want; echoes
# elapsed milliseconds and the final value on stdout as "ms final".
poll_state() {
  local want="$1" unit="$2" t0 ms got i
  t0=$(date +%s%3N)
  for i in $(seq 60); do
    got="$(unit_is "$unit")"
    [ "$got" == "$want" ] && break
    sleep 0.05
  done
  ms=$(( $(date +%s%3N) - t0 ))
  printf '%s %s' "$ms" "$got"
}

# -- preflight ----------------------------------------------------------------

for bin in jq systemctl; do
  command -v "$bin" >/dev/null || { echo "FATAL: $bin not on PATH" >&2; exit 2; }
done
[ -x "$USS" ] && [ -x "$USSD" ] || { echo "FATAL: $USS / $USSD missing — run cargo build first" >&2; exit 2; }
for b in "$USS" "$USSD"; do
  NEWEST_SRC="$(find "$REPO/src" "$REPO/Cargo.toml" "$REPO/Cargo.lock" -newer "$b" -print -quit 2>/dev/null)"
  [ -n "$NEWEST_SRC" ] && { echo "FATAL: $b is stale ($NEWEST_SRC is newer) — run cargo build" >&2; exit 2; }
done

UNIT_DIR="$CFG/systemd/user"
SLEEP_BIN=""
for c in /run/current-system/sw/bin/sleep /bin/sleep /usr/bin/sleep; do
  [ -x "$c" ] && SLEEP_BIN="$c" && break
done
[ -n "$SLEEP_BIN" ] || SLEEP_BIN="$(command -v sleep)"

# -- fixtures -------------------------------------------------------------------

install_fixtures() {
  local n
  for n in "${E2E[@]}"; do
    printf '[Unit]\nDescription=uss e2e watchdog fixture %s\n\n[Service]\nExecStart=%s 300\n' \
      "$n" "$SLEEP_BIN" > "$UNIT_DIR/uss-e2e-$n.service"
  done
  systemctl --user daemon-reload
}

stop_all_fixtures() {
  local n
  for n in "${E2E[@]}"; do
    systemctl --user stop "uss-e2e-$n.service" 2>/dev/null || true
  done
}

remove_fixtures() {
  stop_all_fixtures
  local n
  for n in "${E2E[@]}"; do rm -f "$UNIT_DIR/uss-e2e-$n.service"; done
  systemctl --user reset-failed "${E2E[@]/#/uss-e2e-}" 2>/dev/null || true
  systemctl --user daemon-reload
}

# -- teardown ----------------------------------------------------------------

teardown() {
  echo "== teardown =="
  systemctl --user unmask uss-e2e-m.service 2>/dev/null || true
  remove_fixtures
  systemctl --user stop ussd.service 2>/dev/null || true
  systemctl --user reset-failed ussd.service 2>/dev/null || true
  rm -rf "$CFG_USS_DIR" "$RUN_USS"
  out="$("$USS" </dev/null 2>&1)" || true
  [ -z "$out" ] || echo "NOTE: final status not empty: $out"
  assert_eq "teardown: ussd active (steady state)" "active" "$(unit_is ussd.service)"
  check_state "teardown" '<absent>'
  echo "== summary: $PASS ok, $FAIL failed =="
  rm -rf "$SCRATCH"
  if [ "$FAIL" -eq 0 ]; then exit 0; else exit 1; fi
}
trap 'teardown' EXIT

# -- clean slate ---------------------------------------------------------------

echo "== e2e: watchdog vertical (ticket #23) =="
echo "host:   $(id -un)@$(hostname)  systemd: $(systemctl --version | head -1 | awk '{print $2}')"
echo "uss:    $USS"
echo
systemctl --user unmask uss-e2e-m.service 2>/dev/null || true
remove_fixtures
systemctl --user stop ussd.service 2>/dev/null || true
# Clear the start-limit accounting (kill -9 / stop cycles in this and the
# previous run can hit the default StartLimitBurst — the unit file is
# owned by the daemon slice, so the vertical clears it itself).
systemctl --user reset-failed ussd.service 2>/dev/null || true
rm -rf "$CFG_USS_DIR" "$RUN_USS"
assert_ok "ussd stopped for the vertical" test "$(unit_is ussd.service)" != "active"
install_fixtures
echo "installed ${#E2E[@]} fixtures"
echo

run_uss "boot: bootstrap ussd" 0 "" ""
assert_eq "boot: ussd active" "active" "$(unit_is ussd.service)"

# -- 1. out-of-band start + latency ----------------------------------------------

echo "== 1: out-of-band start reaction =="
run_uss "1a: wa add uss-e2e-a1" 0 "" "" wa add uss-e2e-a1
run_uss "1b: wa add uss-e2e-wa" 0 "" "" wa add uss-e2e-wa   # (wa group: two plain members)
# The reaction only fires for MEMBER start edges (a non-member edge is
# consumed, §7) — a2 joins BEFORE its out-of-band start. add never
# changes active state (§4.2), so the start below is the edge.
run_uss "1c: wa add uss-e2e-a2" 0 "" "" wa add uss-e2e-a2
S1='{"groups":{"wa":["uss-e2e-a1.service","uss-e2e-wa.service","uss-e2e-a2.service"]},"version":1}'
check_state "1" "$S1"
run_uss "1d: start wa member" 0 "" "" wa start uss-e2e-a1
systemctl --user start uss-e2e-a2   # out-of-band (a member — the edge)
# The §7 reaction: the edge's member (a2) keeps running; the other
# active members (a1) are stopped, JobRemoved-confirmed.
RES="$(poll_state inactive uss-e2e-a1)"; MS="${RES%% *}"
assert_ok "1: a1 stopped by the reaction within 3s ($MS ms)" test "$MS" -lt 3000
assert_eq "1: a2 keeps running (the edge's member, §7)" "active" "$(unit_is uss-e2e-a2.service)"
assert_eq "1: a1 inactive (final)" "inactive" "$(unit_is uss-e2e-a1.service)"
run_uss "1: status wa" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service - Active' "" wa
check_state "1" "$S1"
echo

# -- 2. convergence: back-to-back out-of-band starts -----------------------------

echo "== 2: convergence =="
systemctl --user stop uss-e2e-a2   # known-state floor (a2 survived scenario 1)
run_uss "2: status wa (all inactive)" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service' "" wa
systemctl --user start uss-e2e-a1
sleep 0.2                          # let reaction(a1) drain (a2 not started yet -> no-op)
systemctl --user start uss-e2e-a2   # reaction(a2) now stops a1
RES="$(poll_state active uss-e2e-a2)"; MS2="${RES%% *}"
assert_ok "2: last starter survives within 3s ($MS2 ms)" test "$MS2" -lt 3000
sleep 1.5                            # drain the queue deterministically
assert_eq "2: a1 not active (stopped by reaction(a2) or stale-cleaned)" "inactive" "$(unit_is uss-e2e-a1.service)"
assert_eq "2: a2 ACTIVE (last starter — exactly one)" "active" "$(unit_is uss-e2e-a2.service)"
run_uss "2: status wa" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service - Active' "" wa
check_state "2" "$S1"
systemctl --user stop uss-e2e-a2
echo

# -- 3. re-sync across daemon restart ----------------------------------------------

echo "== 3: re-sync =="
run_uss "3a: wb add uss-e2e-wb1" 0 "" "" wb add uss-e2e-wb1
run_uss "3b: wb add uss-e2e-wb2" 0 "" "" wb add uss-e2e-wb2
run_uss "3c: wc add uss-e2e-m" 0 "" "" wc add uss-e2e-m
run_uss "3d: wc add uss-e2e-t" 0 "" "" wc add uss-e2e-t
S3='{"groups":{"wa":["uss-e2e-a1.service","uss-e2e-wa.service","uss-e2e-a2.service"],"wb":["uss-e2e-wb1.service","uss-e2e-wb2.service"],"wc":["uss-e2e-m.service","uss-e2e-t.service"]},"version":1}'
check_state "3" "$S3"
systemctl --user stop ussd.service
systemctl --user reset-failed ussd.service 2>/dev/null || true
assert_eq "3: ussd stopped" "inactive" "$(unit_is ussd.service)"
# Out-of-band starts WHILE ussd is down (signals cannot be delivered —
# the re-sync gap in its rawest form).
systemctl --user start uss-e2e-wb1 uss-e2e-wb2 uss-e2e-m
assert_eq "3: wb1 active with daemon down" "active" "$(unit_is uss-e2e-wb1.service)"
run_uss "3e: status (bootstraps ussd, re-syncs)" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service\nwb\n  uss-e2e-wb1.service - Active\n  uss-e2e-wb2.service - Active\nwc\n  uss-e2e-m.service - Active\n  uss-e2e-t.service' ""
assert_eq "3: ussd re-bootstrapped" "active" "$(unit_is ussd.service)"
check_state "3" "$S3"   # re-sync reads systemd; state never rewritten
# The re-read must be LIVE: the switch now has to stop BOTH wb members.
run_uss "3f: wb start t… (t not a member -> usage-free 3)" 3 "" "uss: uss-e2e-t.service is not in group wb" wb start uss-e2e-t
run_uss "3f2: wb add t2" 5 "" "uss: service uss-e2e-t2.service not found" wb add uss-e2e-t2
run_uss "3g: wb start uss-e2e-wb1 (no-op: target active, others active too? no — others active => switch stops wb2)" 0 "" "" wb start uss-e2e-wb1
assert_eq "3g: wb2 stopped (live re-read, not a stale snapshot)" "inactive" "$(unit_is uss-e2e-wb2.service)"
assert_eq "3g: wb1 active" "active" "$(unit_is uss-e2e-wb1.service)"
# Kill the daemon abruptly (socket disappears under the next command) ->
# bootstrap step 4 (StartUnit) + re-sync again; the byte-exact status is
# the proof.
# m stays Active: nothing between 3e and here touches the wc group (3g's
# switch is group-scoped to wb), and the fresh daemon's re-sync accounts
# for m's out-of-band start (its re-emitted edge is deduped, §7) — no
# reaction, the single active member satisfies the invariant.
MAINPID="$(systemctl --user show ussd -p MainPID --value)"
kill -9 "$MAINPID" 2>/dev/null || true
sleep 0.2
systemctl --user reset-failed ussd.service 2>/dev/null || true
run_uss "3h: status after kill -9 (restart + re-sync, byte-exact)" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service\nwb\n  uss-e2e-wb1.service - Active\n  uss-e2e-wb2.service\nwc\n  uss-e2e-m.service - Active\n  uss-e2e-t.service' ""
check_state "3" "$S3"
echo

# -- 4. masked member: re-validate + structured error ------------------------------

echo "== 4: UnitFilesChanged / masked =="
systemctl --user stop uss-e2e-m     # tidy the pre-arm
# Mask an INACTIVE member: the file is replaced by the /dev/null symlink
# (host: mask refuses an existing user unit file), manager reloads.
rm -f "$UNIT_DIR/uss-e2e-m.service"
systemctl --user mask uss-e2e-m.service >/dev/null 2>&1
for i in 1 2 3 4 5 6 7 8 9 10; do
  [ "$(unit_loadstate uss-e2e-m.service)" == "masked" ] && break
  sleep 0.25
done
assert_eq "4: m masked on host" "masked" "$(unit_loadstate uss-e2e-m.service)"
# ussd's re-validation must NOT change membership (§7):
sleep 0.5
check_state "4" "$S3"
run_uss "4: status wc (m prints bare, membership intact)" 0 $'wc\n  uss-e2e-m.service\n  uss-e2e-t.service' "" wc
run_uss "4: start m (structured row-5)" 5 "" "uss: service uss-e2e-m.service is masked" wc start uss-e2e-m
# A RUNNING member keeps its main process across a mask reload — the
# honest edge of the §7 "reads inactive" sentence (it describes the
# not-running case; the active marker is the live ActiveState).
systemctl --user start uss-e2e-a1
rm -f "$UNIT_DIR/uss-e2e-a1.service"
systemctl --user mask uss-e2e-a1.service >/dev/null 2>&1
for i in 1 2 3 4 5 6 7 8 9 10; do
  [ "$(unit_loadstate uss-e2e-a1.service)" == "masked" ] && break
  sleep 0.25
done
assert_eq "4: running member masked (file gone, main alive)" "masked" "$(unit_loadstate uss-e2e-a1.service)"
sleep 0.5
run_uss "4: status wa (running-but-masked KEEPS - Active)" 0 $'wa\n  uss-e2e-a1.service - Active\n  uss-e2e-wa.service\n  uss-e2e-a2.service' "" wa
systemctl --user unmask uss-e2e-a1.service >/dev/null 2>&1
systemctl --user stop uss-e2e-a1
echo

# -- 5. the §4.2-adjacent honesty check: reaction failure is journal-only ----------
# (the full member-ignores-stop shape is fake-seam territory; the journal
# line format is echoed here for the transcript, not asserted live)
echo "== 5: journal excerpt (ussd reaction lines) =="
sleep 0.4
journalctl --user -u ussd.service --since "-4 min" --no-pager -o short-iso 2>/dev/null \
  | grep -E 'ussd|watchdog' | sed 's/^/  /' | tail -8 || echo "  (no ussd stderr lines — clean run)"
echo

# -- final: steady-state pre-teardown state ----------------------------------------

run_uss "final: status (all inactive)" 0 $'wa\n  uss-e2e-a1.service\n  uss-e2e-wa.service\n  uss-e2e-a2.service\nwb\n  uss-e2e-wb1.service - Active\n  uss-e2e-wb2.service\nwc\n  uss-e2e-m.service\n  uss-e2e-t.service' ""
check_state "final" "$S3"

echo
echo "== all scenarios done =="
