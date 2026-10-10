//! The real `SystemdCtl` backend (spec §3, ADR-0002): one persistent
//! connection to `org.freedesktop.systemd1` on the **user session bus**,
//! over `zbus` + `zbus_systemd`. No `systemctl` shelling anywhere.
//!
//! # Why `EnqueueUnitJob` carries the operations
//!
//! The seam's [`JobHandle`] is the numeric job id — the `JobRemoved`
//! correlation key core matches on (spec §3). `StartUnit`/`StopUnit` reply
//! only with the job *object path*; reading the job's `Id` property after
//! the reply races the job settling (a stop of an inactive unit completes
//! in the same manager iteration, and the job object is gone before the
//! read). `EnqueueUnitJob(name, type, mode)` is the same manager-side
//! transaction with the id in the reply — verified on the maintainer host
//! (systemd 260.4, 2026-10-10): `StopUnit(u, "fail")` and
//! `EnqueueUnitJob(u, "stop", "fail")` against a unit with a queued job
//! fail with byte-identical `TransactionIsDestructive` errors, so the
//! documented Start/StopUnit semantics (§3) hold unchanged.
//!
//! # Grounded D-Bus error names (host probes, systemd 260.4, 2026-10-10)
//!
//! - `TransactionIsDestructive` — a `"fail"`-mode call whose transaction
//!   would displace a queued job (the §4.2 conflicting-job case). Mapped
//!   to [`CtlError::ConflictingJob`] so the core's stop rule sees it.
//! - `NoSuchUnit` — `GetUnit` for a unit with no file. In
//!   [`SystemdCtl::get_unit_state`] this is *data*: `not-found`/`inactive`
//!   (§10 loadability is a state check, matching the fake), not an error.
//! - `org.freedesktop.DBus.Error.FileNotFound` — `GetUnitFileState` for a
//!   non-existent unit file (a plain rejection pass-through).
//! - `AlreadySubscribed` — a repeated `Subscribe()` on the same connection
//!   (research branch, v262 `method_subscribe`): tolerated as `Ok` — the
//!   signal gate is already open.
//!
//! # Divergence from the fake (deliberate, documented)
//!
//! Real systemd *merges* a same-type job: `StopUnit(u, "fail")` while a
//! *stop* job is queued returns the existing job instead of rejecting;
//! the fake rejects on any pending job. The fake is the stricter §4.2
//! model; core is correct either way (it waits on the returned id, and a
//! merged job is the same job whose `JobRemoved` core already awaits).
//!
//! Second divergence (review, 2026-10-10): the fake's `start_unit`/
//! `stop_unit` accept a never-seen unit and return `Ok` (no existence
//! check); the real backend rejects `EnqueueUnitJob` for an unknown unit
//! with `Rejected { name: org.freedesktop.systemd1.NoSuchUnit }` (live
//! probe). Narrow in practice — §10 gates every member operation on a
//! prior `get_unit_state` — but core tests must not assume start/stop of
//! an absent unit succeeds against a real bus.
//!
//! # Signal pumps are loss-free by construction
//!
//! One pump task per connection forwards every `JobRemoved`, every unit
//! `PropertiesChanged` edge, and every unit-file change (the manager's
//! `UnitFilesChanged` plus a unit's `Reloading` property dropping to
//! `false`) onto the seam's broadcast channels. The
//! pump loop never exits on a bad message (skip + continue), and D-Bus
//! applies broker-side backpressure when the per-match queue fills — so
//! delivery stalls rather than drops. Receiver lag still drops *the lagging
//! receiver's* events (broadcast semantics): the spec's answer to that is
//! re-sync (§3), which the core implements — same rule as the fake.
//!
//! A stall inside a pump that itself needs the stalled reader would be a
//! deadlock, not backpressure: zbus 5.19's single socket-reader task
//! awaits each match-rule broadcast *before* reading the next socket
//! message (review, source-verified). So the pumps issue **no method
//! calls inline** — the `ActiveState` invalidated re-read runs on a
//! detached task, keeping the reader draining and the reply reachable.
//!
//! # Reconnect is runtime-orchestrated (ticket #13 decision)
//!
//! The backend does not reconnect on its own: calls on a dead connection
//! return [`CtlError::ManagerAbsent`], the seam's `connection_lost` (impl
//! on this type) notifies when the connection drops, and `connect()`
//! re-establishes (re-spawning pumps) — it also re-verifies the manager
//! name on a still-open connection and abandons one whose manager died.
//! Method calls carry a 30 s timeout (`METHOD_TIMEOUT`, review
//! 2026-10-10): a wedged-but-alive manager fails its calls with a
//! `timeout` rejection instead of freezing the single-task daemon. The
//! ussd runtime adds backoff and drives the
//! `subscribe` + `list_states` re-sync (spec §3: re-sync is mandatory).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Mutex, Notify, broadcast};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream};
use zbus_systemd::systemd1::{ManagerProxy, UnitProxy};

use crate::systemdctl::{
    ActiveState, CtlError, JobHandle, JobRemoved, JobResult, LoadState, SystemdCtl, UnitState,
    UnitStateChanged,
};

/// Re-`Subscribe` on an already-subscribed connection (v262
/// `method_subscribe`); tolerated — see module docs.
pub const ERR_ALREADY_SUBSCRIBED: &str = "org.freedesktop.systemd1.AlreadySubscribed";
/// A `"fail"`-mode transaction that would displace a queued job
/// (host-grounded 2026-10-10): the §4.2 conflicting-job rejection.
pub const ERR_TX_DESTRUCTIVE: &str = "org.freedesktop.systemd1.TransactionIsDestructive";
/// A unit operation on a non-existent unit (`StopUnit`, `GetUnit`; the
/// same rejection also lands when the unit exists on disk but is not
/// loaded — host-grounded 2026-10-10/11).
pub const ERR_NO_SUCH_UNIT: &str = "org.freedesktop.systemd1.NoSuchUnit";
/// `GetUnitFileState` for a non-existent unit file (host-grounded
/// 2026-10-10; note: a plain D-Bus name, not a systemd1 one).
pub const ERR_FILE_NOT_FOUND: &str = "org.freedesktop.DBus.Error.FileNotFound";

const MANAGER: &str = "org.freedesktop.systemd1";
const UNIT_PATH_PREFIX: &str = "/org/freedesktop/systemd1/unit/";
/// Method-call timeout (review, 2026-10-10): a wedged-but-alive manager
/// must not freeze the daemon — the daemon is a single task, and a
/// method call awaiting its reply forever would freeze the command loop
/// (and SIGTERM handling with it). zbus defaults to no timeout; with
/// this one, a stalled call fails with a `timeout` rejection instead.
const METHOD_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-receiver and per-pump-channel capacity (same as the fake's). The
/// pump consumes without blocking beyond the `send`, so real queues stay
/// shallow; overflow means a receiver was far behind and must re-sync.
const SIGNAL_CAPACITY: usize = 128;

// ---------------------------------------------------------------------------
// wire-string maps (pure, unit-tested without a bus)
// ---------------------------------------------------------------------------

/// `JobRemoved` result string → [`JobResult`] (spec §3).
pub(crate) fn job_result_from_wire(s: &str) -> JobResult {
    match s {
        "done" => JobResult::Done,
        "skipped" => JobResult::Skipped,
        "canceled" => JobResult::Canceled,
        "timeout" => JobResult::Timeout,
        "dependency" => JobResult::Dependency,
        "failed" => JobResult::Failed,
        other => JobResult::Other(other.to_owned()),
    }
}

/// Unit `LoadState` property → [`LoadState`] (spec §10).
pub(crate) fn load_state_from_wire(s: &str) -> LoadState {
    match s {
        "loaded" => LoadState::Loaded,
        "stub" => LoadState::Stub,
        "not-found" => LoadState::NotFound,
        "masked" => LoadState::Masked,
        other => LoadState::Other(other.to_owned()),
    }
}

/// Unit `ActiveState` property → [`ActiveState`] (spec §4.3/§7).
pub(crate) fn active_state_from_wire(s: &str) -> ActiveState {
    match s {
        "active" => ActiveState::Active,
        "activating" => ActiveState::Activating,
        "inactive" => ActiveState::Inactive,
        "deactivating" => ActiveState::Deactivating,
        "failed" => ActiveState::Failed,
        other => ActiveState::Other(other.to_owned()),
    }
}

/// Inverse of systemd's unit-name path escaping (`.`→`_2e`, `@`→`_40`,
/// `-`→`_2d`, `_`→`_5f`, …): take the single escaped segment under
/// `/org/freedesktop/systemd1/unit/` and decode `_XX` hex pairs. systemd
/// escapes per **byte of the name's UTF-8 encoding**, so the decoded
/// bytes are re-assembled as UTF-8 (a review-found Latin-1 mistake made
/// non-ASCII names — accepted by `names.rs` — mojibake into never-
/// matching edge keys). `None` on a foreign prefix, malformed escape,
/// or invalid UTF-8 (the pump skips such signals).
fn unit_name_from_dbus_path(path: &str) -> Option<String> {
    let escaped = path.strip_prefix(UNIT_PATH_PREFIX)?;
    // A unit name is one escaped segment; anything deeper is not a unit.
    if escaped.is_empty() || escaped.contains('/') {
        return None;
    }
    let bytes = escaped.as_bytes();
    let mut raw = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' {
            if i + 3 > bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            raw.push(byte);
            i += 3;
        } else {
            raw.push(bytes[i]); // unescaped chars are ASCII by construction
            i += 1;
        }
    }
    String::from_utf8(raw).ok()
}

/// The canonical D-Bus path of a unit object (inverse of
/// [`unit_name_from_dbus_path`]): systemd's `unit_escape` — every
/// non-alphanumeric byte of the name (byte-wise over its UTF-8 encoding)
/// is replaced by `_xx` (lowercase hex: `.`→`_2e`, `-`→`_2d`, `_`→`_5f`).
///
/// Property reads at this path LOAD the unit on demand and report
/// `LoadState` as data — `not-found` when no unit file exists — which is
/// exactly what spec §10's loadability check needs. The
/// `Manager.GetUnit` method cannot serve that: it rejects with
/// `NoSuchUnit` for every unit not currently loaded in the manager,
/// including installed-but-inactive ones (host-grounded, systemd 260,
/// ticket #20 — the first-vertical e2e caught it).
fn unit_object_path(name: &str) -> String {
    let hex = b"0123456789abcdef";
    let mut escaped = String::with_capacity(name.len() * 3);
    for &b in name.as_bytes() {
        if b.is_ascii_alphanumeric() {
            escaped.push(b as char);
        } else {
            escaped.push('_');
            escaped.push(hex[(b >> 4) as usize] as char);
            escaped.push(hex[(b & 0x0f) as usize] as char);
        }
    }
    format!("{UNIT_PATH_PREFIX}{escaped}")
}

// ---------------------------------------------------------------------------
// error mapping (pure core + a thin zbus wrapper, unit-tested without a bus)
// ---------------------------------------------------------------------------

/// The structured D-Bus error name on a method error, if it is one.
fn err_name(err: &zbus::Error) -> Option<String> {
    match err {
        zbus::Error::MethodError(name, _, _) => Some(name.to_string()),
        _ => None,
    }
}

/// Map a failed manager call: a structured rejection carries through as
/// `Rejected` with its name + message (spec §3 "not stderr text" — this
/// *is* the structured form); anything else (transport, protocol) means
/// the bus/manager went away from under us.
fn map_call_error(err: zbus::Error) -> CtlError {
    match err {
        zbus::Error::MethodError(name, message, _) => CtlError::Rejected {
            name: name.to_string(),
            message: message.unwrap_or_default(),
        },
        // A method call that got no reply within `METHOD_TIMEOUT` — the
        // manager is alive (name owned, socket open) but not responding;
        // fail the call instead of freezing the caller.
        zbus::Error::InputOutput(err) if err.kind() == ErrorKind::TimedOut => CtlError::Rejected {
            name: "timeout".into(),
            message: format!("no reply from the user manager within {METHOD_TIMEOUT:?}"),
        },
        _ => CtlError::ManagerAbsent,
    }
}

/// The §3/§4.2 mapping: a `"fail"`-mode stop rejected because the
/// transaction would displace a queued job becomes `ConflictingJob`.
fn remap_stop_rejection(unit: &str, err: CtlError) -> CtlError {
    match err {
        CtlError::Rejected { ref name, .. } if name == ERR_TX_DESTRUCTIVE => {
            CtlError::ConflictingJob {
                unit: unit.to_owned(),
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// the backend
// ---------------------------------------------------------------------------

/// The live connection state; replaced wholesale on (re)connect.
#[derive(Debug)]
struct Shared {
    /// A handle kept for abandoning the connection (a still-open socket
    /// whose manager name is gone — see `install`'s liveness check).
    conn: Connection,
    manager: ManagerProxy<'static>,
    pumps: tokio::task::JoinHandle<()>,
}

/// Real [`SystemdCtl`] backend on `zbus` (module docs cover the design).
#[derive(Debug)]
pub struct ZbusCtl {
    state: Mutex<Option<Shared>>,
    job_tx: broadcast::Sender<JobRemoved>,
    state_tx: broadcast::Sender<UnitStateChanged>,
    files_tx: broadcast::Sender<()>,
    /// Fires when the current connection closes; `lost_now` lets a caller
    /// that arrives after the fact see it immediately.
    lost: Arc<Notify>,
    lost_now: Arc<AtomicBool>,
}

impl Default for ZbusCtl {
    fn default() -> Self {
        Self::new()
    }
}

impl ZbusCtl {
    pub fn new() -> Self {
        let (job_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (state_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (files_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        Self {
            state: Mutex::new(None),
            job_tx,
            state_tx,
            files_tx,
            lost: Arc::new(Notify::new()),
            lost_now: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The current live manager proxy, or `ManagerAbsent` when there is no
    /// connection or it died (no silent reconnect here — see module docs).
    async fn manager(&self) -> Result<ManagerProxy<'static>, CtlError> {
        self.state
            .lock()
            .await
            .as_ref()
            .filter(|s| !s.manager.inner().connection().is_closed())
            .map(|s| s.manager.clone())
            .ok_or(CtlError::ManagerAbsent)
    }

    /// Establish the connection from a finished bus-connect attempt:
    /// absent user manager (bus unreachable, or `org.freedesktop.systemd1`
    /// nameless on it) → `ManagerAbsent` (spec §3 environment edge, §8
    /// step 1). Idempotent: an already-live connection is kept — but only
    /// while the manager name is still owned on it: a still-open socket
    /// whose manager DIED (the name vanishes without the connection
    /// closing) is abandoned — the socket is closed, the pumps retire,
    /// and the new connection attempt re-checks the name. Installs the
    /// signal pumps for the new connection and retires the old task.
    async fn install(&self, conn: Result<Connection, zbus::Error>) -> Result<(), CtlError> {
        let mut state = self.state.lock().await;
        if let Some(shared) = state.take() {
            let existing = shared.manager.inner().connection();
            let manager_gone = existing.is_closed() || !manager_name_owned(existing).await;
            if !manager_gone {
                // Live connection with the manager present; a stale
                // `lost_now` (a pump that exited while its connection was
                // replaced) must not latch.
                state.replace(shared);
                self.lost_now.store(false, Ordering::Release);
                return Ok(());
            }
            // Abandon: closing the socket ends the pumps' streams (their
            // `run_pumps` join completes and raises the lost event); abort
            // + await prove the old pumps are dead before the new
            // connection installs and clears `lost_now`. `state` stays
            // taken; the new connection is installed below.
            let _ = shared.conn.close().await;
            shared.pumps.abort();
            let _ = shared.pumps.await;
        }
        let conn = conn.map_err(|_| CtlError::ManagerAbsent)?;
        let dbus = zbus::fdo::DBusProxy::new(&conn)
            .await
            .map_err(|_| CtlError::ManagerAbsent)?;
        let manager_name =
            zbus::names::BusName::try_from(MANAGER).map_err(|_| CtlError::ManagerAbsent)?; // infallible constant
        if !dbus
            .name_has_owner(manager_name)
            .await
            .map_err(|_| CtlError::ManagerAbsent)?
        {
            return Err(CtlError::ManagerAbsent);
        }
        let manager = ManagerProxy::new(&conn)
            .await
            .map_err(|_| CtlError::ManagerAbsent)?;
        let conn_handle = conn.clone();
        let pumps = tokio::spawn(run_pumps(
            conn,
            self.job_tx.clone(),
            self.state_tx.clone(),
            self.files_tx.clone(),
            self.lost.clone(),
            self.lost_now.clone(),
        ));
        if let Some(old) = state.replace(Shared {
            conn: conn_handle,
            manager,
            pumps,
        }) {
            // Retire the old pumps BEFORE clearing `lost_now`: a pump
            // mid-poll sets the flag in its final poll and `abort`
            // cannot preempt one; awaiting the handle proves it is dead,
            // so no stale store can follow the clear below.
            old.pumps.abort();
            let _ = old.pumps.await;
        }
        self.lost_now.store(false, Ordering::Release);
        Ok(())
    }

    /// `EnqueueUnitJob` → [`JobHandle`] (module docs: why this call).
    async fn enqueue(&self, unit: &str, job_type: &str, mode: &str) -> Result<JobHandle, CtlError> {
        let manager = self.manager().await?;
        let (id, _job_path, _unit, _unit_path, _type, _affected) = manager
            .enqueue_unit_job(unit.to_owned(), job_type.to_owned(), mode.to_owned())
            .await
            .map_err(map_call_error)?;
        Ok(JobHandle { id: id as u64 })
    }
}

/// Whether the manager name is still owned on a (live) connection
/// (`org.freedesktop.systemd1` → bus name lookup). `false` on any error
/// — conservative: an unverifiable manager is treated as gone (the
/// reconnect sequence re-checks).
async fn manager_name_owned(conn: &Connection) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(MANAGER) else {
        return false; // infallible constant
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// Forward the three signal streams for one connection; ends when all
/// streams end (streams only end when the connection closes —
/// per-message problems are skipped inside the loops), which is the
/// connection-lost event.
async fn run_pumps(
    conn: Connection,
    job_tx: broadcast::Sender<JobRemoved>,
    state_tx: broadcast::Sender<UnitStateChanged>,
    files_tx: broadcast::Sender<()>,
    lost: Arc<Notify>,
    lost_now: Arc<AtomicBool>,
) {
    let manager = match ManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(_) => {
            lost_now.store(true, Ordering::Release);
            lost.notify_waiters();
            return;
        }
    };
    let job_pump = pump_job_removed(manager.clone(), job_tx);
    let props_pump = pump_properties_changed(conn, state_tx, files_tx.clone());
    let files_pump = pump_unit_files_changed(manager, files_tx);
    tokio::join!(job_pump, props_pump, files_pump);
    lost_now.store(true, Ordering::Release);
    lost.notify_waiters();
}

/// Every `JobRemoved` → the seam channel (spec §3). `send` only fails when
/// no receiver holds the channel or a receiver lagged — both are the
/// receiver's re-sync concern, not a pump exit.
async fn pump_job_removed(manager: ManagerProxy<'static>, tx: broadcast::Sender<JobRemoved>) {
    let Ok(mut stream) = manager.receive_job_removed().await else {
        return;
    };
    while let Some(signal) = stream.next().await {
        let Ok(args) = signal.args() else {
            continue; // malformed: skip, keep pumping
        };
        let _ = tx.send(JobRemoved {
            id: args.id as u64,
            unit: args.unit.clone(),
            result: job_result_from_wire(&args.result),
        });
    }
}

/// Unit `ActiveState` `PropertiesChanged` edges → the seam channel
/// (spec §7), and a unit's `Reloading` property dropping to `false`
/// (a reload finished) → the unit-file-change channel (spec §7). Only
/// unit objects under `/org/freedesktop/systemd1/unit/` and only the
/// `org.freedesktop.systemd1.Unit` interface (where `ActiveState` is
/// declared); an invalidated (value-less) `ActiveState` is re-read so the
/// edge still carries a state.
async fn pump_properties_changed(
    conn: Connection,
    tx: broadcast::Sender<UnitStateChanged>,
    files_tx: broadcast::Sender<()>,
) {
    let Ok(rule) = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(MANAGER)
        .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
        .and_then(|b| b.member("PropertiesChanged"))
        .map(|b| b.build())
    else {
        return;
    };
    let Ok(mut stream) = MessageStream::for_match_rule(rule, &conn, Some(SIGNAL_CAPACITY)).await
    else {
        return;
    };
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        let Some(path) = header.path().map(|p| p.as_str()) else {
            continue;
        };
        if !path.starts_with(UNIT_PATH_PREFIX) {
            continue;
        }
        let Ok((iface, changed, invalidated)) =
            msg.body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            continue;
        };
        if iface != "org.freedesktop.systemd1.Unit" {
            continue;
        }
        let Some(unit) = unit_name_from_dbus_path(path) else {
            continue;
        };
        // A single `PropertiesChanged` may carry several properties
        // (systemd coalesces changes per unit) — so `ActiveState` and
        // `Reloading` in the same signal are BOTH handled, not
        // early-continued after the first (review, 2026-10-10): a reload
        // finishing in the same manager iteration as a state transition
        // must not lose the `Reloading(false)` event.
        if changed
            .get("Reloading")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            == Some(false)
        {
            // A unit reload finished — the watchdog re-validates member
            // unit files (spec §7).
            let _ = files_tx.send(());
        }
        if let Some(value) = changed.get("ActiveState") {
            if let Ok(state) = value.downcast_ref::<&str>() {
                let _ = tx.send(UnitStateChanged {
                    unit: unit.clone(),
                    active_state: active_state_from_wire(state),
                });
            }
        }
        if invalidated.iter().any(|p| p == "ActiveState") {
            // Value invalidated without a new value: re-read so the edge
            // carries a state; a failed re-read just drops this edge
            // (the next signal or the receiver's re-sync covers it).
            // Detached on purpose: an inline call on this same connection
            // would deadlock whenever the socket reader is blocked
            // broadcasting to this stream (module docs, review-found).
            let conn = conn.clone();
            let tx = tx.clone();
            let path_owned = path.to_owned();
            tokio::spawn(async move {
                let reread = async {
                    let proxy = UnitProxy::builder(&conn)
                        .path(zbus::zvariant::OwnedObjectPath::try_from(path_owned.as_str()).ok()?)
                        .ok()?
                        .build()
                        .await
                        .ok()?;
                    proxy.active_state().await.ok()
                };
                if let Some(state) = reread.await {
                    let _ = tx.send(UnitStateChanged {
                        unit,
                        active_state: active_state_from_wire(&state),
                    });
                }
            });
        }
    }
}

/// The manager's `UnitFilesChanged` signal → the unit-file-change channel
/// (spec §7) — unit files changed on disk: the watchdog re-validates
/// member unit files (newly masked/removed).
async fn pump_unit_files_changed(manager: ManagerProxy<'static>, tx: broadcast::Sender<()>) {
    let Ok(rule) = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(MANAGER)
        .and_then(|b| b.interface("org.freedesktop.systemd1.Manager"))
        .and_then(|b| b.member("UnitFilesChanged"))
        .map(|b| b.build())
    else {
        return;
    };
    let Ok(mut stream) =
        MessageStream::for_match_rule(rule, manager.inner().connection(), Some(SIGNAL_CAPACITY))
            .await
    else {
        return;
    };
    while let Some(msg) = stream.next().await {
        let Ok(_) = msg else { continue }; // bad message: skip, keep pumping
        let _ = tx.send(());
    }
}

impl SystemdCtl for ZbusCtl {
    async fn connect(&self) -> Result<(), CtlError> {
        // `method_timeout` (module docs): the default is no timeout, and a
        // wedged manager would then freeze the single-task daemon.
        let builder = zbus::connection::Builder::session().map_err(|_| CtlError::ManagerAbsent)?;
        self.install(builder.method_timeout(METHOD_TIMEOUT).build().await)
            .await
    }

    async fn subscribe(&self) -> Result<(), CtlError> {
        let manager = self.manager().await?;
        match manager.subscribe().await {
            Ok(()) => Ok(()),
            // Gate already open for this connection — same state we want.
            Err(e) if err_name(&e).as_deref() == Some(ERR_ALREADY_SUBSCRIBED) => Ok(()),
            Err(e) => Err(map_call_error(e)),
        }
    }

    async fn start_unit(&self, unit: &str) -> Result<JobHandle, CtlError> {
        // "replace": a pending job is replaced (settles `canceled`), so
        // start never rejects for a queued job (spec §3).
        self.enqueue(unit, "start", "replace").await
    }

    async fn stop_unit(&self, unit: &str) -> Result<JobHandle, CtlError> {
        // "fail": conflicts reject (spec §3) — mapped to ConflictingJob.
        self.enqueue(unit, "stop", "fail")
            .await
            .map_err(|e| remap_stop_rejection(unit, e))
    }

    fn job_removed(&self) -> broadcast::Receiver<JobRemoved> {
        self.job_tx.subscribe()
    }

    fn unit_state_changed(&self) -> broadcast::Receiver<UnitStateChanged> {
        self.state_tx.subscribe()
    }

    fn unit_files_changed(&self) -> broadcast::Receiver<()> {
        self.files_tx.subscribe()
    }

    /// The seam's liveness primitive (spec §6 daemon reconnect trigger):
    /// resolves when the live connection has dropped. A drop that already
    /// happened (since the last `connect`) returns immediately; if never
    /// connected, waits for the first connection to establish and then
    /// drop. Registers the waiter *before* consulting the flag, so a drop
    /// racing this call cannot be missed (`notify_waiters` stores no
    /// permit).
    async fn connection_lost(&self) {
        let mut notified = std::pin::pin!(self.lost.notified());
        notified.as_mut().enable(); // register now, consume permit on await
        if self.lost_now.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    async fn get_unit_state(&self, unit: &str) -> Result<UnitState, CtlError> {
        let manager = self.manager().await?;
        // The canonical unit object path (see [`unit_object_path`]): the
        // property reads load the unit on demand, and `LoadState` is
        // data — `not-found` when no unit file exists, like the fake.
        let unit_proxy = UnitProxy::builder(manager.inner().connection())
            .path(unit_object_path(unit))
            .map_err(|_| CtlError::ManagerAbsent)? // invalid by construction = broken manager
            .build()
            .await
            .map_err(map_call_error)?;
        let active = unit_proxy.active_state().await.map_err(map_call_error)?;
        let load = unit_proxy.load_state().await.map_err(map_call_error)?;
        Ok(UnitState {
            load_state: load_state_from_wire(&load),
            active_state: active_state_from_wire(&active),
        })
    }

    async fn list_states(&self, units: &[String]) -> Result<Vec<(String, UnitState)>, CtlError> {
        let mut out = Vec::with_capacity(units.len());
        for unit in units {
            out.push((unit.clone(), self.get_unit_state(unit).await?));
        }
        Ok(out)
    }

    async fn get_unit_file_state(&self, unit: &str) -> Result<String, CtlError> {
        let manager = self.manager().await?;
        // Unknown file rejects with `org.freedesktop.DBus.Error.FileNotFound`
        // (host-grounded) — a `Rejected` pass-through, name included.
        manager
            .get_unit_file_state(unit.to_owned())
            .await
            .map_err(map_call_error)
    }

    async fn reload(&self) -> Result<(), CtlError> {
        let manager = self.manager().await?;
        manager.reload().await.map_err(map_call_error)
    }

    async fn enable_unit_files(&self, units: &[String]) -> Result<(), CtlError> {
        let manager = self.manager().await?;
        // runtime=false (persistent symlinks), force=true (spec §8 step 3);
        // the returned change list is not surfaced by the seam.
        manager
            .enable_unit_files(units.to_vec(), false, true)
            .await
            .map_err(map_call_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::{Instant, timeout};
    use zbus::connection::Builder;
    use zbus::zvariant::Value;

    // -- pure wire/path/error mapping (no bus) -------------------------------

    #[test]
    fn wire_maps_cover_every_documented_string() {
        assert_eq!(job_result_from_wire("done"), JobResult::Done);
        assert_eq!(job_result_from_wire("skipped"), JobResult::Skipped);
        assert_eq!(job_result_from_wire("canceled"), JobResult::Canceled);
        assert_eq!(job_result_from_wire("timeout"), JobResult::Timeout);
        assert_eq!(job_result_from_wire("dependency"), JobResult::Dependency);
        assert_eq!(job_result_from_wire("failed"), JobResult::Failed);
        assert_eq!(
            job_result_from_wire("weird"),
            JobResult::Other("weird".into())
        );
        assert_eq!(load_state_from_wire("loaded"), LoadState::Loaded);
        assert_eq!(load_state_from_wire("stub"), LoadState::Stub);
        assert_eq!(load_state_from_wire("not-found"), LoadState::NotFound);
        assert_eq!(load_state_from_wire("masked"), LoadState::Masked);
        assert_eq!(
            load_state_from_wire("bogus"),
            LoadState::Other("bogus".into())
        );
        assert_eq!(active_state_from_wire("active"), ActiveState::Active);
        assert_eq!(
            active_state_from_wire("activating"),
            ActiveState::Activating
        );
        assert_eq!(active_state_from_wire("inactive"), ActiveState::Inactive);
        assert_eq!(
            active_state_from_wire("deactivating"),
            ActiveState::Deactivating
        );
        assert_eq!(active_state_from_wire("failed"), ActiveState::Failed);
        assert_eq!(
            active_state_from_wire("what"),
            ActiveState::Other("what".into())
        );
    }

    #[test]
    fn unit_paths_unescape_back_to_names() {
        // The research branch's live example, plus every escape class.
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/gpg_2dagent_2eservice")
                .as_deref(),
            Some("gpg-agent.service")
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/user_401000_2eslice")
                .as_deref(),
            Some("user@1000.slice")
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/foo_5fbar_2eservice")
                .as_deref(),
            Some("foo_bar.service")
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/dev_2dsda_2edevice")
                .as_deref(),
            Some("dev-sda.device")
        );
        // systemd escapes per byte of the UTF-8 name (é = c3 a9) — names.rs
        // accepts non-ASCII members, so multi-byte names must decode whole
        // (regression guard for the Latin-1 unescape review finding).
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/caf_c3_a9_2eservice")
                .as_deref(),
            Some("café.service")
        );
    }

    #[test]
    fn foreign_or_malformed_paths_are_skipped() {
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/manager"),
            None
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/"),
            None
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/a/b"),
            None // multi-segment path: not a unit object
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/bad_zz_2eservice").as_deref(),
            None // _zz is not valid hex
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/bad_ff_2eservice"),
            None // _ff is valid hex but not valid UTF-8
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/trailing_5f").as_deref(),
            Some("trailing_") // _5f is a complete escape for a literal '_'
        );
        assert_eq!(
            unit_name_from_dbus_path("/org/freedesktop/systemd1/unit/trunc_5"),
            None // escape cut short at the end of the path
        );
    }

    #[test]
    fn unit_object_path_escapes_and_round_trips() {
        // Literal escapes (systemd's unit_escape; host-grounded by
        // property reads on these paths, ticket #20).
        assert_eq!(
            unit_object_path("sleep-fixture.service"),
            "/org/freedesktop/systemd1/unit/sleep_2dfixture_2eservice"
        );
        assert_eq!(
            unit_object_path("ussd.service"),
            "/org/freedesktop/systemd1/unit/ussd_2eservice"
        );
        // A literal underscore escapes to `_5f` — never a bare `_`.
        assert_eq!(
            unit_object_path("a_b.service"),
            "/org/freedesktop/systemd1/unit/a_5fb_2eservice"
        );
        // Every escape must decode back to the original name (the pump's
        // edge-key matching depends on the two inverses agreeing),
        // including non-ASCII names escaped byte-wise.
        for name in [
            "sleep-fixture.service",
            "a.service",
            "foo@1.service",
            "x-y_z.target",
            "héllo.service",
        ] {
            let decoded = unit_name_from_dbus_path(&unit_object_path(name));
            assert_eq!(decoded.as_deref(), Some(name));
        }
    }

    #[test]
    fn non_method_errors_are_manager_absent() {
        // Transport/protocol failures mean "bus gone", not "rejected".
        assert_eq!(
            map_call_error(zbus::Error::Unsupported),
            CtlError::ManagerAbsent
        );
    }

    #[test]
    fn only_the_destructive_transaction_becomes_conflicting_job() {
        let destructive = CtlError::Rejected {
            name: ERR_TX_DESTRUCTIVE.into(),
            message: "transaction".into(),
        };
        assert_eq!(
            remap_stop_rejection("a.service", destructive),
            CtlError::ConflictingJob {
                unit: "a.service".into()
            }
        );
        let other = CtlError::Rejected {
            name: ERR_NO_SUCH_UNIT.into(),
            message: "not loaded".into(),
        };
        assert_eq!(remap_stop_rejection("a.service", other.clone()), other);
        assert_eq!(
            remap_stop_rejection("a.service", CtlError::ManagerAbsent),
            CtlError::ManagerAbsent
        );
    }

    #[tokio::test]
    async fn connecting_to_an_unreachable_bus_is_manager_absent() {
        // No session bus reachable: the connect attempt fails and install
        // maps it to the §3 environment-edge error (no unsafe set_var —
        // `#![forbid(unsafe_code)]` crate-wide — install takes the attempt).
        let ctl = ZbusCtl::new();
        let attempt = Builder::address("unix:path=/definitely-not-a-bus-t17.sock")
            .expect("valid address syntax")
            .build()
            .await;
        assert!(attempt.is_err(), "bogus address must not connect");
        assert_eq!(ctl.install(attempt).await, Err(CtlError::ManagerAbsent));
    }

    // -- real user manager (opt-in; spec §11 integration) --------------------
    //
    // Run on a host with a live user session: `cargo test -- --ignored`.
    // Fixture units are transient oneshot sleeps that settle/cancel
    // themselves — nothing user-owned is touched, and no enable/install is
    // exercised here (that mutates user config; the bootstrap e2e slice
    // owns that ground).

    fn sleep_bin() -> String {
        std::process::Command::new("sh")
            .args(["-c", "command -v sleep"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| "/run/current-system/sw/bin/sleep".to_owned())
    }

    /// Borrowed value → owned D-Bus variant (transient-unit properties).
    fn ovalue<T>(v: T) -> OwnedValue
    where
        T: Into<Value<'static>> + zbus::zvariant::DynamicType,
    {
        Value::new(v).try_into_owned().expect("owned round-trip")
    }

    fn probe_unit(tag: &str) -> String {
        format!("uss-t17-{tag}-{}.service", std::process::id())
    }

    async fn fresh_ctl() -> ZbusCtl {
        let ctl = ZbusCtl::new();
        ctl.connect().await.expect("real user manager");
        ctl.subscribe().await.expect("Subscribe");
        ctl
    }

    /// A second, plain manager connection for test *setup* (transient
    /// units, CancelJob) — out-of-band from the backend under test.
    async fn out_of_band_manager() -> ManagerProxy<'static> {
        let conn = Connection::session().await.expect("session bus");
        ManagerProxy::new(&conn).await.expect("manager proxy")
    }

    async fn start_oneshot_probe(
        mgr: &ManagerProxy<'static>,
        unit: &str,
        secs: &str,
        remain: bool,
    ) {
        // Transient ExecStart carries full ExecCommand structs `a(sasb)`
        // (path, argv, ignore-failure) — v260 dbus-execute.c
        // bus_set_transient_exec_command (source-grounded 2026-10-10;
        // plain `as`/`s` values are rejected as unexpected contents).
        let path = sleep_bin();
        let mut props = vec![
            ("Type".to_owned(), ovalue("oneshot")),
            (
                "ExecStart".to_owned(),
                ovalue(vec![(path.clone(), vec![path, secs.to_owned()], false)]),
            ),
        ];
        if remain {
            props.push(("RemainAfterExit".to_owned(), ovalue(true)));
        }
        mgr.start_transient_unit(unit.to_owned(), "replace".to_owned(), props, vec![])
            .await
            .expect("transient oneshot start");
    }

    /// Cancel whatever job the probe unit currently has (cleanup).
    async fn cancel_probe_job(mgr: &ManagerProxy<'static>, unit: &str) {
        if let Ok(jobs) = mgr.list_jobs().await {
            for (id, _, job_unit, _, _, _) in jobs {
                if job_unit == unit {
                    let _ = mgr.cancel_job(id).await;
                }
            }
        }
    }

    async fn wait_for_state(ctl: &ZbusCtl, unit: &str, want: ActiveState, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let st = ctl.get_unit_state(unit).await.unwrap();
            if st.active_state == want {
                return;
            }
            assert!(Instant::now() < deadline, "unit never reached {want:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires a live user manager (maintainer host)"]
    async fn real_status_and_unit_file_reads() {
        let ctl = fresh_ctl().await;
        // Re-subscribing must be tolerated (AlreadySubscribed is the gate
        // already being open for this connection, module docs).
        ctl.subscribe().await.expect("AlreadySubscribed tolerated");

        // dbus-broker owns the session bus we are standing on: it is
        // loaded + active on any host where this test can connect at all.
        let st = ctl.get_unit_state("dbus-broker.service").await.unwrap();
        assert_eq!(st.load_state, LoadState::Loaded);
        assert_eq!(st.active_state, ActiveState::Active);

        // A unit with no file is data (§10), like the fake.
        let ghost = ctl
            .get_unit_state("uss-t17-definitely-absent.service")
            .await
            .unwrap();
        assert_eq!(ghost.load_state, LoadState::NotFound);
        assert_eq!(ghost.active_state, ActiveState::Inactive);

        let file_state = ctl
            .get_unit_file_state("dbus-broker.service")
            .await
            .unwrap();
        assert!(!file_state.is_empty(), "a real unit file has a state");

        let err = ctl
            .get_unit_file_state("uss-t17-definitely-absent.service")
            .await
            .unwrap_err();
        match err {
            CtlError::Rejected { name, .. } => assert_eq!(name, ERR_FILE_NOT_FOUND),
            other => panic!("expected a structured rejection, got {other:?}"),
        }

        // Synchronous Reload (bootstrap step 3, spec §8): returns on completion.
        ctl.reload().await.expect("Manager.Reload");
    }

    #[tokio::test]
    #[ignore = "requires a live user manager (maintainer host)"]
    async fn real_stop_cycle_correlates_job_removed_and_state_edges() {
        let unit = probe_unit("stop");
        let ctl = fresh_ctl().await;
        let mut rx_job = ctl.job_removed();
        let mut rx_state = ctl.unit_state_changed();
        let mgr = out_of_band_manager().await;

        start_oneshot_probe(&mgr, &unit, "3", true).await; // becomes active, stays
        let mut edges = Vec::new();
        let start_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            while let Ok(edge) = rx_state.try_recv() {
                if edge.unit == unit {
                    edges.push(edge.active_state);
                }
            }
            let st = ctl.get_unit_state(&unit).await.unwrap();
            if st.active_state == ActiveState::Active {
                break;
            }
            assert!(Instant::now() < start_deadline, "probe never went active");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // The out-of-band start's PropertiesChanged arrived through the pump
        // (grounds path unescaping + interface filter, spec §7).
        assert!(
            edges
                .iter()
                .any(|s| matches!(s, ActiveState::Active | ActiveState::Activating)),
            "missing start edge, saw {edges:?}"
        );

        let handle = ctl.stop_unit(&unit).await.expect("stop accepted");
        // JobRemoved for exactly our job id (grounds the EnqueueUnitJob
        // id ↔ JobRemoved id correlation, spec §3).
        let result = loop {
            let removed = timeout(Duration::from_secs(10), rx_job.recv())
                .await
                .expect("JobRemoved in time")
                .expect("channel open");
            if removed.id == handle.id {
                assert_eq!(removed.unit, unit);
                break removed.result;
            }
        };
        assert!(result.is_success(), "stop settled with {result}");

        // A stop edge (deactivating or inactive) reached the state channel.
        let stop_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let edge = timeout(Duration::from_millis(500), rx_state.recv())
                .await
                .ok()
                .transpose()
                .unwrap_or_default();
            if let Some(edge) = edge {
                if edge.unit == unit
                    && matches!(
                        edge.active_state,
                        ActiveState::Deactivating | ActiveState::Inactive
                    )
                {
                    break;
                }
            }
            assert!(Instant::now() < stop_deadline, "missing stop edge");
        }

        // After settling: not-active, and (once systemd unloads the
        // transient unit) not-found reads back as data.
        wait_for_state(&ctl, &unit, ActiveState::Inactive, 10).await;
        drop(mgr);
    }

    #[tokio::test]
    #[ignore = "requires a live user manager (maintainer host)"]
    async fn real_conflicting_stop_maps_to_conflicting_job() {
        let unit = probe_unit("conflict");
        let ctl = fresh_ctl().await;
        let mut rx_job = ctl.job_removed(); // drain the cancel signal
        let mgr = out_of_band_manager().await;

        // A queued oneshot start keeps the start job pending for the whole
        // sleep window — the §4.2 conflict, live.
        start_oneshot_probe(&mgr, &unit, "6", false).await;
        let err = ctl.stop_unit(&unit).await.unwrap_err();
        assert_eq!(err, CtlError::ConflictingJob { unit: unit.clone() });

        // Cleanup: cancel the queued start (settles `canceled`).
        cancel_probe_job(&mgr, &unit).await;
        let drain_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < drain_deadline {
            match timeout(Duration::from_millis(500), rx_job.recv()).await {
                Ok(Ok(removed)) if removed.unit == unit => break,
                Ok(Ok(_)) | Err(_) => continue,
                Ok(Err(_)) => break,
            }
        }
        drop(mgr);
    }
}
