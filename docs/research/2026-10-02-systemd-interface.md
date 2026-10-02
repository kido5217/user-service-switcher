# Research: how to talk to systemd (CLI vs D-Bus), crates, watchdog detection

**Date:** 2026-10-02 · **Ticket:** #5 (wayfinder) · **Host:** NixOS 26.05, systemd **260.4** user manager, GNOME session (uid 1000)

## TL;DR — recommendation

| Decision | Recommendation |
|---|---|
| **Operation** (uss + ussd → systemd) | **D-Bus API** `org.freedesktop.systemd1` on the **user/session bus** — no `systemctl` shelling. Persistent connection; `StopUnit`/`StartUnit` (mode `"replace"`/`"fail"`), `StartUnitReplace` for queued-job swap; results tracked via the returned job object + `JobRemoved(result)`. |
| **Observation** (watchdog) | **Native D-Bus signals**: `Subscribe()` once, then `PropertiesChanged` on member unit objects (`ActiveState`/`SubState`, both `emits-change`) + `JobRemoved` for op completion. Measured detection latency **~5–6 ms** after an out-of-band start. Re-sync via `ListUnitsByNames`/`GetUnit` on startup and reconnect. Polling `is-active` rejected (latency, races, load-churn side effect — observed). |
| **Crates** | **`zbus` 5.x + `zbus_systemd` 0.26200.x** (`systemd1` feature, `zbus-async-tokio` or `-smol`). Both **pure Rust** — no FFI, no user approval needed under the crate policy. Both compile- and run-verified on this host (below). |
| **Bootstrap** (first use, CLI before daemon exists) | Same D-Bus path from `uss`: write unit file (plain fs) → `Manager.Reload()` (synchronous reply) → `Manager.EnableUnitFiles([unit], runtime=false, force=true)` → `Manager.StartUnit(unit, "replace")` + wait `JobRemoved(result="done")`. Requires a running user manager (session or linger — see edge cases); fail with an explicit message otherwise. |

---

## 1. The systemd D-Bus API for user units

### 1.1 Where it lives

The user manager (`systemd --user`, running as `user@UID.service`) exports the same well-known name as the system manager, `org.freedesktop.systemd1` at `/org/freedesktop/systemd1`, **on the user's session bus** (the "API bus", brokered by `dbus-broker` at `$XDG_RUNTIME_DIR/bus`). Unit objects live at `/org/freedesktop/systemd1/unit/<escaped-name>` (e.g. `gpg-agent.service` → `/org/freedesktop/systemd1/unit/gpg_2dagent_2eservice`; escaping: `.`→`_2e`, `@`→`_40`, `-`→`_2d`, `/`→`_2f`).

Evidence: full `busctl --user introspect` dump of the running 260.4 manager (methods, signatures, signals — see §1.2/§1.3); `ListUnits` output showing object paths; and the live zbus probe connecting with `Connection::session()` and resolving the name (appendix `probe-output.log`).

Additionally the manager listens on a **private socket** `$XDG_RUNTIME_DIR/systemd/private` that `systemctl --user`/`busctl --user` use directly (no broker). This matters for signal semantics (§1.3) and for the fact that `systemctl` never shares your process's bus connection.

### 1.2 Operation methods (from the live 260.4 introspection)

All unit-operation methods take `(name, job_mode)` and return a **job object path**:

```
StartUnit(ss)→o   StopUnit(ss)→o   RestartUnit(ss)→o   TryRestartUnit(ss)→o
ReloadUnit(ss)→o  ReloadOrRestartUnit(ss)→o  ReloadOrTryRestartUnit(ss)→o
StartUnitReplace(old, new, mode)→o
EnqueueUnitJob(name, job_type, job_mode)→(job_id, job_path, unit, unit_path, job_type, affected_jobs)
KillUnit(name, whom, signal)   CancelJob(job_id)   ResetFailedUnit(name)
GetUnit(name)→object path      ListUnits()→a(ssssssouso)   ListUnitsByNames(names)
ListJobs()   GetJob(id)→path   GetUnitFileState(name)→s
```

Job modes (documented in `org.freedesktop.systemd1(5)`, read locally from the systemd 260.4 man store):

| mode | meaning |
|---|---|
| `"replace"` | start the unit + deps, **replacing conflicting queued jobs** |
| `"fail"` | same, but **fail** if it would change an already queued job |
| `"isolate"` | start and terminate everything not a dependency |
| `"ignore-dependencies"` / `"ignore-requirements"` | not recommended by the man page |

**Atomicity of "stop others, then start"** — how it maps onto the API:

- systemd's **job queue is the transaction boundary**: each `StartUnit`/`StopUnit` enqueues one job; the manager serializes actual state changes per job. Issuing stop-then-start from a single writer (ussd) is two method calls on one connection; the manager's job scheduler does the ordering, and ussd is never racing itself (it is the single writer).
- `StartUnitReplace(old_unit, new_unit, mode)` exists for swapping a **queued** job for another unit's job in one manager-side operation (man page: "replaces a job that is queued for one unit by a job for another unit") — useful if ussd's stop job is still queued when the user switches target again.
- `EnqueueUnitJob` additionally reports `affected_jobs` (which queued jobs it displaced) — good for debugging switch races.
- Operation **outcome tracking** is a first-class, documented pattern (man page, `StartUnit()` section): *"Callers that want to track the outcome of the actual start operation need to monitor the result of this job. This can be achieved in a race-free manner by first subscribing to the `JobRemoved()` signal, then calling `StartUnit()` and using the returned job object to filter out unrelated `JobRemoved()` signals."* `JobRemoved` carries `result ∈ {done, canceled, timeout, failed, dependency, skipped}`.
- Errors are **structured D-Bus errors** (`org.freedesktop.systemd1.NoSuchUnit`, `UnitMasked`, …) — no parsing of stderr text or locale-dependent strings, no exit-code archaeology.

**Startup/IPC cost** (measured on this host, 2026-10-02):

| operation | cost |
|---|---|
| one D-Bus method round-trip on a **persistent** connection (Subscribe, StopUnit in the probe) | **~1–2 ms** |
| `systemctl --user is-active` (fresh process + fresh bus connect each time) | **5.3–6.4 ms** per invocation |
| `busctl --user get-property` (fresh process + one call) | 4.2–4.6 ms |

A daemon performing N operations over its lifetime saves N process spawns, N bus connect+auth handshakes, and gains typed errors + job objects. For the watchdog the difference is structural: `systemctl` can deliver **no signals at all** — observation would be forced into polling.

### 1.3 Observation: signals, and the `Subscribe()` gate

Signals emitted by the user manager on the session bus (live introspection + `org.freedesktop.systemd1(5)`):

| signal | semantics |
|---|---|
| `UnitNew(name, path)` / `UnitRemoved(name, path)` | unit **loaded/unloaded into memory** — *not* state changes (man page is explicit) |
| `JobNew(id, job, unit)` / `JobRemoved(id, job, unit, result)` | job queued / dequeued |
| `UnitFilesChanged()` | enabled/masked unit files on disk changed |
| `Reloading(bool)` | before/after a daemon reload |
| `StartupFinished(6×u64)` | startup finished |
| `PropertiesChanged` (standard D-Bus, per object) | unit `ActiveState`/`SubState`/`LoadState`/… transitions — all `emits-change` (verified in the live unit-object introspection) |

**The gate** (this is the critical feasibility fact): the man page says *"Signals are only sent out if at least one client invoked this method"* about `Subscribe()`, and the v262 source confirms the mechanism:

- `method_subscribe` (`src/core/dbus-manager.c`): *"Note that direct bus connection subscribe by default, we only track peers on the API bus here"* — peers on the **session bus** are tracked with an `sd_bus_track` (per-connection, refcounted, auto-cleaned when the connection closes: *signals stop as soon as all subscribed clients closed their connection or called `Unsubscribe()`*).
- Emission (`src/core/dbus.c` `bus_foreach_bus`): *"Send to all direct buses, unconditionally"* vs *"Send to API bus, but only if somebody is subscribed"* (`sd_bus_track_count(m->subscribed) > 0`).
- Consequences for ussd:
  1. ussd (a session-bus client) **must call `Subscribe()`** after connecting — and again after every reconnect. Re-calling while subscribed returns the `org.freedesktop.systemd1.AlreadySubscribed` error; handle it.
  2. Once subscribed, the manager emits to the broker and the **broker fans out to every client** matching its rules — ussd receives them with no per-signal registration.
  3. `systemctl`/`busctl` connect to the **private bus**, which subscribes unconditionally — so `systemctl monitor`-style tools work without `Subscribe()`, which is easy to mistake for session-bus behavior. (Verified: `busctl`'s `monitor` verb never calls `Subscribe` in v262 source.)
  4. Job/Unit signals carry per-job/per-unit `bus_track`s fed only by transient-unit `AddRef`; for normal unit files (our case) the global `Subscribe()` count is the only gate.

**Watchdog detection latency, measured** (out-of-band `systemd-run --user --unit=uss-research-test sleep 20`, probe subscribed on the session bus — appendix `probe-output.log`):

| event | Δ from out-of-band start |
|---|---|
| `UnitNew` (unit loaded) | **+5 ms** |
| `JobNew` (start job) | **+5 ms** |
| `PropertiesChanged` burst on the unit object (dead→activating→active) | +6 ms |
| `JobRemoved … result=done` | **+6 ms** |
| `ActiveState=active` readable via `Properties.Get` | +6 ms |

Polling comparison: a 250 ms poll (the probe's fallback) detects at 250 ms **worst case**, misses fast start+stop sequences entirely (a unit that started and stopped between polls never reads `active` — and the watchdog would wrongly do nothing), and **causes load churn**: every `Get`/`GetUnit` on a not-loaded unit makes the manager load it (observed: repeated `UnitNew`/`UnitRemoved` pairs, one per 100 ms poll, for a vanished transient unit). Signals have none of these defects.

### 1.4 Bootstrap fit (first use: CLI before the daemon exists)

Every bootstrap step is a plain D-Bus call on the user bus, executable by `uss` with no daemon present:

| step | D-Bus |
|---|---|
| write `~/.config/systemd/user/ussd.service` | plain filesystem write (no systemd API) |
| `systemctl --user daemon-reload` | `Manager.Reload()` — **returns a reply on completion** (no `no-reply` flag in the live introspection; `systemctl-daemon-reload.c` calls it synchronously with a long timeout and expects the reply) |
| `systemctl --user enable ussd.service` | `Manager.EnableUnitFiles(files, runtime=false, force=true) → (has_install_info, changes[(type, path, dest)])` (man page: `runtime=false` → persistent symlinks, i.e. `~/.config/systemd/user/…` for user units) |
| `systemctl --user start ussd.service` | `Manager.StartUnit("ussd.service", "replace")` → job; wait `JobRemoved(result="done")` for that job (documented race-free pattern) |
| verify | `GetUnit` + unit `ActiveState` property |

### 1.5 Session / logind edge cases (where the user manager does *not* exist)

From the v262 source (fetched and read 2026-10-02):

- The user manager is started by **logind** in exactly two situations:
  - a **session** of a class that wants the service manager is opened (`user_wants_service_manager`: `SESSION_CLASS_WANTS_SERVICE_MANAGER(s->class)` — display manager, console login, SSH via `pam_systemd`), or
  - the user has **lingering** enabled (`/var/lib/systemd/linger` marker file, read at logind startup by `manager_enumerate_linger_users`; `loginctl enable-linger`).
- `user@.service` (system scope) carries `BindsTo=user-runtime-dir@UID.service` (dies when the runtime dir is removed) and a `After=systemd-user-sessions.service` drop-in (login barrier). After logout of all sessions it is terminated **unless lingering**.
- There is **no on-demand activation** of the user manager by the system manager in v262 (searched the source tree: only logind starts it; `sd_bus_default_user()` has no fallback; `systemctl --user` connects to `$XDG_RUNTIME_DIR/systemd/private` and fails with `-ENOMEDIUM` when `XDG_RUNTIME_DIR` is unset).
- **Design consequence:** `uss` bootstrap must detect the absent user manager (connect failure / no `$XDG_RUNTIME_DIR`) and print an explicit error ("user manager not running — log in to a session or `loginctl enable-linger $USER`") instead of failing cryptically. In the normal use case (desktop user managing user services) the manager is present. Inside a session, both the session bus (`$DBUS_SESSION_BUS_ADDRESS` / `$XDG_RUNTIME_DIR/bus`) and the private socket exist.
- **ussd's lifetime is bounded by the user manager's** (ussd *is* a user unit): if the manager stops, ussd stops with it — there is nothing left to watch, and state must be re-established on next start (hence the startup re-sync in §2.2).

## 2. Rust crate landscape (checked 2026-10-02, from crates.io + GitHub)

| crate | kind | latest | last activity | status |
|---|---|---|---|---|
| **`zbus`** (z-galaxy) | pure Rust, full D-Bus 1.x stack (protocol, auth, objects, signals, async + blocking APIs) | **5.19.0** (2026-08-09) | last commit **2026-10-02** (same day as this research) | **active** |
| **`zbus_systemd`** (lucab) | pure Rust, **auto-generated proxies for all systemd D-Bus services**, on zbus | **0.26200.0** (2026-09-29) — version-locked to systemd releases (0.262xx ↔ systemd v262) | last commit 2026-09-29 (release PR) | **active**; author is **Luca Bruno, the author of zbus** |
| `dbus` (dbus-rs, diwic) | pure Rust, older general D-Bus stack | 0.9.12 (2026-07-03) | commit 2026-09-09 | active but general-purpose, not systemd-shaped |
| `systemd-zbus` (de-vri-es) | zbus-based systemd proxies | 5.3.2 (2025-05) | repo last commit **2022-12-25** | abandoned |
| `libsystemd-sys` / `systemd` (codyps/rust-systemd) | **FFI** bindings to libsystemd (sd-bus & co.) | 0.9.4 / 0.10.1 (2025-07-19) | repo commit 2025-10-03 | stagnant; **FFI → explicit user approval required by crate policy** |
| `systemd-dbus` (hugoduncan) | pure Rust "thin systemd wrapper" | **0.0.1 (2015-12-11)** | last updated 2015; 3k lifetime downloads | dead |
| `async_bus` | minimal async D-Bus client | 0.1.0 (2022-04-21) | repo-less | dead |

### 2.1 `zbus` (the D-Bus layer)

- Pure Rust implementation of the D-Bus 1.x + object protocol; no libdbus/libsystemd. Session/system bus builders: `Connection::session()` / `Connection::system()` (async), `zbus::blocking::Connection::session()` (blocking).
- Async is the primary API (since 2.0); a blocking API exists and can be disabled (`blocking-api` feature). Runtime-agnostic: `zbus-async-tokio` or `zbus-async-smol` (default `async-io`), or bring your own.
- Typed proxies via the `#[proxy]` macro; `MessageStream`/`SignalStream` for signals; `PropertiesChanged` helpers; match rules (`MatchRule`); well-known-name tracking (`NameOwnerChanged`) built into signal streams.
- Verified on this host: compiled and run twice (appendix probes); API shapes used below.

### 2.2 `zbus_systemd` (the systemd layer)

- `systemd1` cargo feature generates `ManagerProxy` (destination `org.freedesktop.systemd1`, path `/org/freedesktop/systemd1`) plus `Unit`, `Service`, `Socket`, … proxies, auto-generated from systemd's interface definitions — so it tracks upstream API drift (v0.26200.0 shipped with systemd v262).
- Typed API surface needed by uss/ussd (verified by compiling `docs/research/probe-async-live.rs` and running it live, 2026-10-02):

```rust
let conn = Connection::session().await?;              // user bus
let manager = ManagerProxy::new(&conn).await?;
manager.subscribe().await?;                           // watchdog gate (§1.3)
manager.reload().await?;                              // bootstrap daemon-reload
manager.enable_unit_files(vec!["ussd.service".into()], false, true).await?; // → (bool, Vec<(String,String,String)>)
let job: OwnedObjectPath = manager.start_unit("ussd.service".into(), "replace".into()).await?;
let job: OwnedObjectPath = manager.stop_unit("a.service".into(), "replace".into()).await?;
let job: OwnedObjectPath = manager.start_unit_replace("a.service".into(), "b.service".into(), "replace".into()).await?;
let unit_path: OwnedObjectPath = manager.get_unit("ussd.service".into()).await?;
let unit = UnitProxy::builder(&conn).path(unit_path.clone())?.build().await?;
let state: String = unit.active_state().await?;       // + sub_state(), load_state(), …
let units = manager.list_units().await?;
let mut job_removed = manager.inner().receive_signal("JobRemoved").await?; // SignalStream<Item = Message>
// generated signal methods also exist: #[zbus(signal)] unit_new/unit_removed/job_new/job_removed/…
```

- Live run output (appendix `probe-output.log`): `Manager.Version = 260.4`, `GetUnit` → `/org/freedesktop/systemd1/unit/dbus_2dbroker_2eservice`, `ActiveState=active SubState=running LoadState=loaded`, `ListUnits → 494 units`.
- MSRV 1.87.0 (0.26200.0); depends on `zbus ≥5.3` with default features off (enable `zbus-async-tokio` or `zbus-async-smol`); MIT/Apache-2.0.
- Caveats observed: `UnitProxy` for a member must be rebuilt after the unit object path changes (new load → same path for a given name, so stable); `PropertiesChanged` for a unit comes from the `org.freedesktop.DBus.Properties` interface — consume it with a match rule on the unit path or `receive_all_signals` on the unit proxy; the blocking API's `MessageIterator` must be polled continuously (a blocked signal thread keeps the process alive — observed in the appendix probe; irrelevant for the async daemon).

## 3. Why not `systemctl --user` (failure modes)

1. **No observation channel at all** — every call is a fresh process + fresh bus connection to the private socket; there is nothing to watch, so the watchdog would have to poll, with the latency/race/churn defects in §1.3.
2. **Text/stderr + exit-code protocol**: errors are human-facing strings (locale-dependent) and exit codes; no job objects, no structured error names, no way to await a specific job's outcome.
3. **Cost**: 5.3–6.4 ms floor per call (process spawn + connect + auth) vs ~1–2 ms in-process; × every operation, forever, for a daemon.
4. **No atomic batch primitive**: "stop others then start" is N separate processes; the manager still serializes the jobs, but ussd loses the single-writer single-connection discipline (two `systemctl` processes could interleave with an out-of-band actor in ways a single connection's ordered calls do not).
5. Bootstrap works fine with `systemctl` too — but the daemon needs D-Bus regardless (for the watchdog), so a D-Bus-only stack avoids maintaining two code paths. `systemctl` remains a reasonable *debugging* interface; the product should not depend on it.

## 4. Design implications for ussd (from the evidence)

- **One persistent session-bus connection** (zbus async, tokio). On start: connect → `Subscribe()` (tolerate `AlreadySubscribed`) → re-sync state: `GetUnit` per member (loads if needed) + read `ActiveState`; then watch `PropertiesChanged` on member unit objects and `JobRemoved` globally.
- **Switch (the `start` command's op)**: for each other active member `StopUnit(name, "fail")` — use `"fail"` so a conflicting queued job surfaces as a structured error instead of being silently replaced — then `StartUnit(target, "replace")`; confirm each via `JobRemoved` on the returned job object. `StartUnitReplace` is the escalation path when a queued job is being swapped.
- **Watchdog reaction** to `PropertiesChanged(ActiveState ∈ {activating, active})` on a member not started by ussd: treat as out-of-band start → stop the group's other active members, keep the newcomer.
- **Re-sync, don't trust history**: signals are not replayed. On bus disconnect/reconnect (zbus surfaces connection state), and on daemon start, re-read all member states. `Reloading(false)` + `UnitFilesChanged` → re-validate member unit files (masked/removed members).
- **Headless edge**: bootstrap detects missing user manager (no `$XDG_RUNTIME_DIR`, connect failure) → actionable error (session or `loginctl enable-linger`).
- **No FFI anywhere**: `zbus` + `zbus_systemd` are pure Rust; the FFI route (`libsystemd-sys`) is rejected both on policy (per-crate approval) and on merit (stagnant since mid-2025).

## 5. Sources

**Primary (read this session):**
- Live `busctl --user introspect` of the running 260.4 user manager (Manager + Unit interfaces; signatures, signals, `no-reply` flags, `emits-change` properties) — host: uid 1000 GNOME session, `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus`.
- systemd **260.4** man pages from the Nix store: `org.freedesktop.systemd1(5)` (Subscribe/Unsubscribe gate, job modes, `StartUnit` job-tracking pattern, signal semantics, `EnableUnitFiles`), `user@.service(5)`, `systemd(1)`, `systemd.special(7)`, `busctl(1)`.
- systemd **v262** source (fetched 2026-10-02): `src/core/dbus-manager.c` (`method_subscribe`, per-connection `sd_bus_track`), `src/core/dbus.c` (`bus_foreach_bus` emission gate), `src/core/dbus-unit.c` (per-unit `bus_track`, `AddRef`), `src/login/logind-user.c` (`user_start_service_manager`, `user_wants_service_manager`), `src/login/logind.c` (`manager_enumerate_linger_users`), `src/shared/bus-util.c` (`bus_connect_user_systemd` → private socket), `src/systemctl/systemctl-daemon-reload.c` (synchronous `Reload()`), `units/user@.service.in` (+ `10-login-barrier.conf`), `NEWS` (v262 released 2026-09-22).
- crates.io registry API (`/api/v1/crates/*`) + GitHub (last-commit dates, READMEs, Cargo.toml, generated sources) for: zbus, zbus_systemd, dbus-rs, systemd-zbus, libsystemd-sys, systemd-dbus, async_bus (all 2026-10-02).
- **Empirical** (this host, 2026-10-02): zbus 5.19.0 blocking probe (`docs/research/probe-blocking-signals.rs` + `probe-output.log` — Subscribe, GetUnit polling, out-of-band `systemd-run` start, signal timeline, StopUnit + job tracking); zbus_systemd 0.26200.0 + tokio live probe (`docs/research/probe-async-live.rs` + tail of `probe-output.log`); `systemctl`/`busctl` timing baseline.

**Environment:** host systemd 260.4 (NixOS 26.05), current stable systemd v262 (2026-09-22) — API surfaces used here are stable across that range (the man-page semantics read are from the installed 260.4; the mechanism details from v262 source).
