# uss/ussd — design spec

**Status:** proposed (PR'd for review) · **Date:** 2026-10-02
**Map:** [#4](https://github.com/kido5217/user-service-switcher/issues/4) · **This ticket:** [#6](https://github.com/kido5217/user-service-switcher/issues/6)
**Decisions consolidated from:** map charting (2026-10-02, two grilling rounds), ticket [#5](https://github.com/kido5217/user-service-switcher/issues/5) (systemd-interface research), ADR-0001, ADR-0002, `CONTEXT.md`.

## 1. Purpose and scope

`uss`/`ussd` switch between mutually exclusive systemd **user** services. Services are organized into named **groups**; at most one member of a group may be *active* (running) at any time. `start` performs a **switch**: stop the group's other active members, then start the target. The invariant is enforced continuously by the daemon, not only when the CLI runs.

In scope:

- Linux, systemd, user scope only. Rust.
- `uss` — CLI control tool (thin client).
- `ussd` — systemd user daemon: control plane + watchdog (ADR-0001).
- Group lifecycle, state persistence, bootstrap, error model, testing strategy.

Out of scope (see the map): implementing the tool (this spec is the handoff), `systemctl enable/disable` management (runtime-only by decision — *enabled* is out of the tool's domain), system-scope services, packaging/distribution.

## 2. Architecture

```
 uss (CLI, one-shot per command)
  │  Unix stream socket, JSON lines (one request → one response)
  ▼
 ussd (systemd user service, single writer)
  │  persistent D-Bus connection, user session bus
  ▼
 systemd user manager (org.freedesktop.systemd1)
```

- **uss** parses and validates arguments, ensures the daemon is installed and running (§8), sends exactly one request, prints the result or an error, exits. It never talks to systemd directly except during bootstrap (§8), and never touches the state file.
- **ussd** owns all group state, issues every systemd operation, and runs the watchdog (§7). It is the **single writer**: all state mutations and all systemd operations are serialized through it.
- **State file** `~/.config/uss/groups.json` (§5) stores membership only — never runtime state. Active state is always read live from systemd (point-in-time snapshot); this is the "re-sync, don't trust history" rule from ADR-0002.
- **Watchdog reaction** (settled): an out-of-band start of a group member is the user's intent — ussd stops the group's other active members and keeps the newcomer, exactly as `start` would.

## 3. systemd interface

Settled by ticket #5 and ADR-0002. Summary:

- **No `systemctl` shelling.** Both binaries talk to `org.freedesktop.systemd1` on the **user session bus** over one persistent D-Bus connection (ussd); uss uses the bus only for bootstrap (§8).
- **Crates:** `zbus` 5.x (5.19.0 at time of writing, `zbus-async-tokio` feature) + `zbus_systemd` 0.26200.0 (`systemd1` feature; version-locked to the systemd v262 series) + `tokio`. Both D-Bus crates are **pure Rust — no FFI anywhere**, satisfying the crate policy without approvals. MSRV ≥ 1.87 (zbus_systemd's).
- **Operations:** `StopUnit(name, "fail")` / `StartUnit(name, "replace")`, each returning a job object; outcome tracked race-free via the returned job + `JobRemoved(id, job, unit, result)`. `StartUnitReplace` is the escalation path when a stop job is still queued and the user switches target again. Success results: `done`, `skipped` (no-op operation); anything else (`failed`, `canceled`, `timeout`, `dependency`) is an operation failure (exit 6, §4.4). Errors arrive as structured D-Bus names (`NoSuchUnit`, `UnitMasked`, …), not stderr text.
- **Observation:** `Manager.Subscribe()` once after connecting and after every reconnect (per-connection, refcounted; the manager emits session-bus signals only while ≥1 client is subscribed — a hard gate, source-verified). Then: `PropertiesChanged` on each member's unit object (`ActiveState`/`SubState`) for out-of-band detection, and `JobRemoved` for own-operation outcomes. Measured detection latency ~5–6 ms.
- **Re-sync is mandatory, not a nicety:** signals are not replayed. On startup and on every bus reconnect, ussd re-reads all members' `ActiveState` (`GetUnit` + property read). `UnitFilesChanged`/`Reloading(false)` trigger re-validation of member unit files.
- **Environment edge:** the user manager exists only with a logind session or lingering. Absent manager → explicit error, not a cryptic failure (§4.4, exit 7). ussd's lifetime is bounded by the user manager's; a stopped manager means a stopped ussd, and the next `uss` command restarts it via bootstrap.

## 4. CLI reference

### 4.1 Global behavior

- Parsing via `clap` (derive). `--help`/`--version` standard.
- Every command (including bare `uss`) first runs the daemon-ensure flow (§8).
- Mutations (`add`/`remove`/`start`/`stop`) print **nothing on success**; only `uss` status prints. All errors go to **stderr** with the prefix `uss: ` and set the exit code from §4.4. This keeps stdout clean for scripting: stdout carries status data only.
- `uss` is one-shot: one socket connection per invocation, one request, one response, exit. Concurrent invocations are serialized by ussd (§6).
- Request timeout: uss waits 30 s for the response (a `start` waits for its job; a slow service's start job can take a while). On timeout: exit 7, `uss: ussd unavailable (no response within 30s)`; the operation continues inside ussd and is visible via status.

### 4.2 Commands and semantics

| command | effect |
|---|---|
| `uss` | status of all groups (§4.3). |
| `uss <group>` | status of one group. |
| `uss <group> add <service>` | add a member: normalize + validate the name (§10); the group is **created** if new; reject if the service already belongs to another group (exit 4); verify the unit is loadable (exit 5 class, §10); record and persist membership. Adding does not change the active state of anything. |
| `uss <group> remove <service>` | detach a member; if it is active, stop it first (settled) and persist only after the stop succeeds — a failed stop leaves the member in the group, exit 6, nothing changed. The group is **deleted** when its last member is removed. |
| `uss <group> start <service>` | the **switch**: `StopUnit(name, "fail")` each other active member, and only after all stop jobs have succeeded (`done`/`skipped`) `StartUnit(target, "replace")`; confirm each via `JobRemoved` on the returned job. A stop failure aborts the switch — exit 6, the target is not started, the group's state is unchanged. Target already active and no others active → no-op success. |
| `uss <group> stop <service>` | `StopUnit(target, "fail")`, confirmed via `JobRemoved`. Already inactive → no-op success. The group may then have zero active members (legal). |

Command preconditions: the group must exist (exit 2) and the service must be a member (exit 3) for `remove`/`start`/`stop`. `add` on a service that is already a member of the same group is a no-op success; `add` on a service that belongs to another group fails with exit 4.

### 4.3 Status output

User-fixed format. Point-in-time snapshot: `active` = `ActiveState == "active"` read live from systemd at query time. Transient states (`activating`, `deactivating`, `failed`, …) print bare — no marker.

```
$ uss
dev
  foo.service
vpn
  openvpn.service
  wireguard.service - Active

$ uss vpn
vpn
  openvpn.service - Active
```

- Groups sorted alphabetically, one per line, no indentation.
- Members indented two spaces, in **add order**; the running member gets the ` - Active` suffix.
- No groups → no output, exit 0. `uss <unknown>` → exit 2.

### 4.4 Errors and exit codes

| exit | class | when | message (stderr, prefix `uss: `) |
|---|---|---|---|
| 0 | success | — | — |
| 1 | usage | invalid group/service name syntax, unknown subcommand, extra args (clap handles flag errors with its own messages) | `invalid service name: 'foo.target' (use 'name' or 'name.service')`, `invalid group name: '…'` |
| 2 | unknown group | any command naming a group that does not exist | `no such group: <group>` |
| 3 | not a member | `remove`/`start`/`stop` on a non-member | `<service> is not in group <group>` |
| 4 | conflict | `add` on a service already in another group | `<service> already belongs to group <other>` |
| 5 | service problem | service not found / not loadable / masked (at `add`, or at `start` when the unit turns out unusable) | `service <service> not found`, `service <service> is masked` |
| 6 | systemd operation failed | manager rejected the call (structured D-Bus error) or the job finished unsuccessfully (`failed`, `canceled`, `timeout`, `dependency`) | method rejected: `failed to <start\|stop> <service>: <D-Bus error name> (<message>)`; bad job result: `failed to <start\|stop> <service>: job <result>` |
| 7 | environment | user manager absent (bootstrap), ussd binary missing, ussd unavailable or failed to start, response timeout | `user manager not running — log in to a session or run: loginctl enable-linger $USER`, `uss: ussd binary not found (looked next to uss and in PATH)`, `ussd failed to start — check: journalctl --user -u ussd.service`, `ussd unavailable (<reason>)` |
| 8 | daemon internal | protocol/version mismatch, unexpected internal error | `daemon error: <message>` |

Client-side (uss, before any socket traffic): exit 1 (syntax), exit 7 (user manager absent, ussd binary missing). ussd-side errors arrive as protocol error codes (§6) that uss maps to the table above.

## 5. Group state (`groups.json`)

- **Path:** `$XDG_CONFIG_HOME/uss/groups.json` (default `~/.config/uss/`). Created on demand (dir + file) by ussd; the parent dir is created with mode `0755`, the file `0644`.
- **Schema (v1):** membership only, no runtime state.

```json
{
  "version": 1,
  "groups": {
    "vpn": ["openvpn.service", "wireguard.service"],
    "dev": ["foo.service"]
  }
}
```

  - `groups`: object, key = group name, value = array of member unit names in **add order** (arrays preserve order; group display order is alphabetical anyway, §4.3).
  - All member names are the normalized full form (`*.service`).
  - A missing file means empty state (first use). A corrupt/unknown-`version` file is a hard error: ussd refuses to start (clear log line in the user journal; systemd's start-rate limit then takes the unit to `failed`, ending the restart loop) and `uss` reports exit 7 with the journal pointer (§4.4) — never a silent rewrite or data loss.
- **Writes:** only ussd, only after a successful mutation, atomic — write a temp file in the same directory, then `rename(2)` over the target.
- **Group lifecycle:** object key created on first `add`; key removed when the last member is removed.

## 6. Wire protocol (uss ↔ ussd)

- **Transport:** Unix **stream** socket at `$XDG_RUNTIME_DIR/uss/ussd.sock`. Created by ussd on startup: parent dir `0700`, socket `0600`. Removed on clean shutdown. No systemd socket-activation unit.
- **Framing:** one JSON object per line (newline-terminated). One request per connection from uss; exactly one response from ussd. No streaming, no keepalive.
- **Versioning:** every message carries `"v": 1`. ussd rejects other versions (protocol error, exit 8 class); uss rejects a response version it doesn't know.
- **Requests** (uss → ussd):

```json
{"v":1,"id":1,"cmd":"status"}
{"v":1,"id":1,"cmd":"status","group":"vpn"}
{"v":1,"id":1,"cmd":"add","group":"vpn","service":"openvpn.service"}
{"v":1,"id":1,"cmd":"remove","group":"vpn","service":"openvpn.service"}
{"v":1,"id":1,"cmd":"start","group":"vpn","service":"openvpn.service"}
{"v":1,"id":1,"cmd":"stop","group":"vpn","service":"openvpn.service"}
```

  `id` is 1 for one-shot uss (kept for symmetry/future). `group` is optional only for `status` (absent = all groups).
- **Responses** (ussd → uss):

```json
{"v":1,"id":1,"ok":true,
 "result":{"groups":[
   {"name":"vpn","members":[
     {"name":"openvpn.service","active":true},
     {"name":"wireguard.service","active":false}]}]}}
```

```json
{"v":1,"id":1,"ok":false,"error":"unit-not-loadable","message":"unit openvpn.service not found"}
```

  Stable error codes: `unknown-group` (2), `not-a-member` (3), `service-in-other-group` (4), `unit-not-loadable` (5), `unit-masked` (5), `op-failed` (6), `protocol` (8), `internal` (8). `message` is human-readable and safe to print verbatim. (A corrupt state file is not a protocol error: ussd cannot serve commands at all — exit 7, §5.)
- **Concurrency:** ussd processes commands strictly **sequentially** (one global command lock; status is read-only but still takes the lock — simplest correct rule). Commands are fast (1–2 ms D-Bus round-trips; a switch additionally waits its jobs), so serialization is not a practical bottleneck and preserves the single-writer discipline against races between concurrent `uss` invocations.
- **ussd lifecycle vs. bus:** if the session bus connection drops, ussd does not exit — it reconnects with backoff, re-`Subscribe()`s, and re-syncs (§3). (User-manager death ends ussd too; bootstrap handles the rest.)

## 7. Watchdog

- **Detection:** `PropertiesChanged` on each member's unit object. A member entering `activating`/`active` is the start edge (settled: out-of-band start = user's intent). Entering `inactive`/`failed` requires no reaction (zero active is legal).
- **Own-operation bookkeeping:** ussd records every job it issues (job object path → unit + purpose). A state change that matches one of ussd's in-flight start jobs is ussd's own doing — the watchdog ignores it. `JobRemoved` settles the job: result ∈ {done, skipped} fulfills the pending command's promise; anything else fails the command (exit 6 to the waiting uss).
- **Out-of-band reaction:** member X of group G becomes active without an ussd job behind it → for every other active member of G: `StopUnit("fail")`, confirmed via `JobRemoved`. X keeps running. **Stale edges are discarded:** a start edge is executed against the *live* state at execution time — if the member is no longer active (e.g. just stopped by a prior reaction), the edge is dropped. Out-of-band starts on G therefore converge: after the queue drains, exactly one active member remains — the one whose edge was processed last among the non-stale ones (in practice, the last starter). If a watchdog stop itself fails (a member that ignores stop), the failure is logged to the journal; the group stays double-active until the next start edge re-asserts the invariant.
- **Reaction to `UnitFilesChanged` / `Reloading(false)`:** re-validate member unit files (detect newly masked/removed units). No automatic membership changes — a masked or vanished unit simply reads inactive and surfaces structured errors at the next `start`/`add`.
- **Re-sync:** on startup and after every bus reconnect, re-read all members' `ActiveState` before re-enabling signal-driven reactions (signals are not replayed).
- **Latency:** signal-driven, ~5–6 ms measured (ticket #5 evidence). No polling anywhere.

## 8. Bootstrap (first use)

Runs at the start of **every** `uss` command; idempotent. All steps are D-Bus on the user session bus (ADR-0002) — `systemctl` is never shelled.

1. **User manager present?** Connect to the session bus and resolve `org.freedesktop.systemd1`. No `$XDG_RUNTIME_DIR`, no bus, or no manager name → exit 7 with the `loginctl enable-linger` message (§4.4).
2. **Locate the ussd binary:** sibling of the running `uss` executable, else `$PATH`. Not found → exit 7, `uss: ussd binary not found (looked next to uss and in PATH)`.
3. **Unit installed and current?** `GetUnitFileState("ussd.service")` plus a byte-compare of the installed file against what bootstrap would write (§9). If not `enabled`, or the content differs: write the unit file → `Manager.Reload()` (synchronous reply) → `Manager.EnableUnitFiles(["ussd.service"], runtime=false, force=true)` (persistent symlinks) → proceed.
4. **Daemon running?** Unit `ActiveState`. If not active: `StartUnit("ussd.service", "replace")`, wait for its `JobRemoved(result ∈ {done, skipped})`, verify `ActiveState == "active"`. Failure → exit 7, `ussd failed to start — check: journalctl --user -u ussd.service` (the journal names the cause, e.g. a corrupt state file, §5).
5. **Socket ready:** connect to `$XDG_RUNTIME_DIR/uss/ussd.sock` (retry up to 2 s — ussd creates it during startup). Not connectable → exit 7.
6. Send the command (§6).

Notes: the byte-compare in step 3 covers a moved binary and spec revisions without needless reloads. Enabling ussd makes it come back at login; a plain restart of the user manager (reboot) also restores it — state in `groups.json` is untouched either way.

## 9. ussd unit file

Written by bootstrap to `~/.config/systemd/user/ussd.service`:

```ini
[Unit]
Description=uss — user service switcher daemon
Documentation=https://github.com/kido5217/user-service-switcher/blob/main/docs/spec/uss-ussd.md

[Service]
ExecStart=<absolute path of the ussd binary, resolved in §8 step 2>
Restart=on-failure
RestartSec=1s

[Install]
WantedBy=default.target
```

- `WantedBy=default.target` → enabled units start with the user session.
- `Restart=on-failure` (a clean stop — logout, `stop ussd.service` — is not a failure; the next `uss` command restarts it via §8 step 4 either way).
- No `After=`/`Wants=`: ussd has no ordering needs beyond the user manager, which runs all user units.
- The binary path is absolute at write time; a moved binary is corrected by the next bootstrap run (§8 step 3).
- No `.socket` unit: ussd owns its socket; systemd socket activation is not used.

## 10. Naming and validation

- **Service names (CLI input):** `foo` → `foo.service`; `foo.service` accepted verbatim; anything else (other suffixes, empty, multiple dots not ending in `.service`) → exit 1 with the usage message. After normalization, a light local unit-name syntax check (no leading `-`, no NUL/whitespace, length ≤ 255) before any D-Bus call.
- **Loadability check (`add`, and at `start` if not already known):** `GetUnit(name)` → `LoadState`: `loaded`/`stub` → ok; `not-found` → exit 5 (`service <name> not found`); `masked` → exit 5 (`service <name> is masked`).
- **Group names:** non-empty, ≤ 64 bytes, no whitespace, no `/`, no NUL. Group names are local file keys only — no systemd interpretation. Invalid → exit 1.
- **Normalization everywhere:** storage and all output use the full `*.service` form.

## 11. Testing strategy

- **Seam:** a `SystemdCtl` trait in the library (connect, `subscribe`, `start_unit`/`stop_unit` → job handles, `job_removed` stream, `get_unit_state`, `list_states`, `reload`, `enable_unit_files`) with two implementations: the real zbus backend and an in-memory **fake** (manual state injection, recorded jobs, signal push). ussd's core (state machine, switch logic, watchdog) takes the trait — all logic testable without systemd.
- **Unit tests (always run):** name normalization/validation; `groups.json` round-trip, atomic write, corrupt-file refusal, group create/delete lifecycle; protocol encode/decode + version mismatch; exit-code mapping; switch/watchdog state machines against the fake (out-of-band start → others stopped; own-job suppression; re-sync on reconnect; back-to-back out-of-band starts converge to exactly one active member — no zero-active, no lingering double-active).
- **CLI tests (always run):** a fake-ussd harness speaking the protocol on a temp socket → assert exit codes, stderr messages, and stdout status formatting for every row of §4.4 and the §4.3 format.
- **Integration (opt-in, `#[ignore]` + a dev-shell script):** real user manager, fixture units (trivial sleep services). Cover: full switch cycle, status accuracy, out-of-band start via `systemd-run --user` → watchdog stops the other member within ~1 s, bootstrap from a clean `~/.config/uss` + unit dir (first use installs and starts ussd), reboot-resilience of state (stop manager, `uss` again).
- **No FFI, no system build deps:** the pure-Rust dependency set builds in the existing nix dev shell; the integration script runs inside a real session (`loginctl`-enabled host).

## 12. Implementation notes

- **Layout:** the existing root crate `user-service-switcher` (edition 2024) gains `src/bin/uss.rs` and `src/bin/ussd.rs`; shared logic (protocol types, state model + file IO, name validation, exit codes, the `SystemdCtl` trait) lives in `src/lib.rs`. MSRV ≥ 1.87.
- **Dependencies (all pure Rust):** `zbus` (feature `zbus-async-tokio`), `zbus_systemd` (feature `systemd1`), `tokio` (rt-multi-thread, time, macros), `clap` (derive), `serde` + `serde_json`, `thiserror`.
- **Nix:** the crate currently builds an empty store output (lib-only). Adding the two bin targets makes `packages.x86_64-linux.default` carry artifacts again — no new system dependencies are needed to *build* (no `-sys` crates).
- **Errors:** `thiserror` enums mirroring the §4.4 classes; the protocol error codes are its serialized form.

## 13. Decision traceability

| decision | source |
|---|---|
| uss = CLI client, ussd = control plane + watchdog, Unix socket | charting round 1 + ADR-0001 |
| watchdog stops the others on an out-of-band start | charting round 2 |
| `start` = stop others then start target; `stop` = stop target | user's original usage spec |
| `remove` stops an active member; one group per service; groups auto-created/deleted | charting rounds 1–2 (+ original spec) |
| runtime-only (no enable/disable management) | charting round 1 |
| `~/.config/uss/groups.json`, machine-owned | charting round 2 |
| auto-install on first use (every command ensures the daemon) | charting round 2 |
| names: bare or `.service`, normalized in output, validated before accepting | charting round 2 |
| status format | user's original usage spec |
| D-Bus on the user session bus, `Subscribe()` + `PropertiesChanged` + `JobRemoved`, ~5–6 ms detection, re-sync rule, no systemctl | ticket #5 research + ADR-0002 |
| `zbus` + `zbus_systemd`, pure Rust, no FFI | ticket #5 research + ADR-0002 + crate policy |
| spec-level (authored here, reviewable): exit-code table, message wording, JSON schema, wire protocol, sequential command queue, 30 s request timeout, unit-file content, binary resolution, group-name rules, no output on mutation success, testing layout | this spec |
