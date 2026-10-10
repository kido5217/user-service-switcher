//! The `SystemdCtl` seam (spec §11): the contract ussd's core is written
//! against, with two implementations — the real zbus backend (a later slice)
//! and the in-memory [`FakeSystemdCtl`] (this file).
//!
//! Mode semantics (grounded in the `org.freedesktop.systemd1` man page):
//!
//! - `start_unit` = `StartUnit(unit, "replace")` — a pending job is
//!   *replaced*: the old job is dequeued with result `canceled` and a new job
//!   is queued. Start is never rejected for a pending job.
//! - `stop_unit` = `StopUnit(unit, "fail")` — rejected with
//!   [`CtlError::ConflictingJob`] while the unit has a queued job (the spec
//!   §4.2 conflicting-job case).
//!
//! Like systemd's signals, the fake's streams have no replay: events pushed
//! before a receiver subscribes are lost (spec §3 re-sync rule).
// Native `async fn` in traits (AFIT) is the deliberate seam shape on edition
// 2024; the lint's complaint about auto trait bounds is the known tradeoff —
// this single-threaded, user-scope daemon does not require `Send` futures.
#![allow(async_fn_in_trait)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// The job result strings systemd reports in `JobRemoved` (spec §3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobResult {
    Done,
    Skipped,
    Canceled,
    Timeout,
    Dependency,
    Failed,
    /// Anything else systemd reports.
    Other(String),
}

impl JobResult {
    /// Success results per spec §3: `done`, `skipped`. Anything else fails
    /// the waiting command (exit 6, §4.4).
    pub fn is_success(&self) -> bool {
        matches!(self, JobResult::Done | JobResult::Skipped)
    }
}

impl fmt::Display for JobResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            JobResult::Done => "done",
            JobResult::Skipped => "skipped",
            JobResult::Canceled => "canceled",
            JobResult::Timeout => "timeout",
            JobResult::Dependency => "dependency",
            JobResult::Failed => "failed",
            JobResult::Other(s) => s,
        })
    }
}

/// A handle to a queued job — the reply of `start_unit`/`stop_unit` (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobHandle {
    /// The systemd job id — the `JobRemoved` correlation key.
    pub id: u64,
}

/// A `JobRemoved` signal (spec §3): job id, unit, result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRemoved {
    pub id: u64,
    pub unit: String,
    pub result: JobResult,
}

/// Unit load state (spec §10 loadability check: `loaded`/`stub` ok,
/// `not-found` exit 5, `masked` exit 5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadState {
    Loaded,
    Stub,
    NotFound,
    Masked,
    /// Anything else systemd reports.
    Other(String),
}

/// Unit `ActiveState` (spec §4.3/§7 — status and the watchdog watch this).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActiveState {
    Active,
    Activating,
    Inactive,
    Deactivating,
    Failed,
    /// Anything else systemd reports.
    Other(String),
}

/// A point-in-time unit state snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    pub load_state: LoadState,
    pub active_state: ActiveState,
}

/// A `PropertiesChanged` edge on a member's `ActiveState` (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitStateChanged {
    pub unit: String,
    /// The new `ActiveState` after the change.
    pub active_state: ActiveState,
}

/// Errors the seam can report.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CtlError {
    /// No user manager on the session bus (bootstrap → exit 7, §4.4).
    #[error("user manager not running")]
    ManagerAbsent,
    /// The manager rejected the call with a structured D-Bus error (spec
    /// §3): name + message for the §4.4 row-6 template.
    #[error("rejected: {name} ({message})")]
    Rejected {
        /// The D-Bus error name, e.g. `org.freedesktop.systemd1.NoSuchUnit`.
        name: String,
        message: String,
    },
    /// The unit already has a queued job; `StopUnit` mode `fail` was
    /// rejected (spec §3/§4.2).
    #[error("conflicting job on {unit}")]
    ConflictingJob { unit: String },
}

/// How a job got into the fake's recorded set (test introspection only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOrigin {
    /// Queued via the seam's `start_unit`.
    Start,
    /// Queued via the seam's `stop_unit`.
    Stop,
    /// Injected by a test via [`FakeSystemdCtl::queue_job`].
    Queued,
}

/// A recorded job (fake test introspection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub handle: JobHandle,
    pub unit: String,
    pub origin: JobOrigin,
}

/// The seam (spec §11). The real zbus backend (a later slice) and the
/// in-memory fake both implement this; ussd's core takes the trait.
pub trait SystemdCtl {
    /// Connect to the user session bus and resolve the systemd1 manager.
    /// Absent manager → [`CtlError::ManagerAbsent`] (bootstrap, §8 step 1).
    async fn connect(&self) -> Result<(), CtlError>;

    /// `Manager.Subscribe()` — once after connecting and after every
    /// reconnect (per-connection, spec §3).
    async fn subscribe(&self) -> Result<(), CtlError>;

    /// `StartUnit(unit, "replace")` (spec §3). A pending job is replaced
    /// (dequeued with result `canceled`), never rejected.
    async fn start_unit(&self, unit: &str) -> Result<JobHandle, CtlError>;

    /// `StopUnit(unit, "fail")` (spec §3). Rejected with
    /// [`CtlError::ConflictingJob`] while the unit has a queued job.
    async fn stop_unit(&self, unit: &str) -> Result<JobHandle, CtlError>;

    /// A receiver for `JobRemoved` signals (spec §3). No replay.
    fn job_removed(&self) -> broadcast::Receiver<JobRemoved>;

    /// A receiver for members' `ActiveState` `PropertiesChanged` edges
    /// (spec §7). No replay.
    fn unit_state_changed(&self) -> broadcast::Receiver<UnitStateChanged>;

    /// A receiver for unit-file change events (spec §7): the manager's
    /// `UnitFilesChanged` signal and a unit's `Reloading` property
    /// dropping to `false` (a reload finished). Payload-free: the
    /// watchdog's reaction (re-validate member unit files) is the same
    /// for either. No replay.
    fn unit_files_changed(&self) -> broadcast::Receiver<()>;

    /// One unit's live state (`GetUnit` + properties, spec §3).
    async fn get_unit_state(&self, unit: &str) -> Result<UnitState, CtlError>;

    /// Batched live state for re-sync (spec §3/§7): all requested units,
    /// input order preserved.
    async fn list_states(&self, units: &[String]) -> Result<Vec<(String, UnitState)>, CtlError>;

    /// `GetUnitFileState(unit)` (bootstrap step 3, spec §8): the unit file's
    /// state string (`enabled`, `disabled`, `masked`, …). An unknown file
    /// rejects with `org.freedesktop.DBus.Error.FileNotFound` (the real
    /// backend's name, host-grounded by the zbus-backend slice).
    async fn get_unit_file_state(&self, unit: &str) -> Result<String, CtlError>;

    /// `Manager.Reload()` (bootstrap step 3, spec §8) — synchronous reply.
    async fn reload(&self) -> Result<(), CtlError>;

    /// `Manager.EnableUnitFiles(units, runtime=false, force=true)`
    /// (bootstrap step 3, spec §8) — persistent symlinks.
    async fn enable_unit_files(&self, units: &[String]) -> Result<(), CtlError>;
}

/// Signal channel capacity. Comfortably above any realistic in-flight batch;
/// a lagging receiver drops its own events (it resyncs, spec §3).
const SIGNAL_CAPACITY: usize = 128;

/// Mutable fake state, guarded so the `&self` trait API can allocate jobs.
#[derive(Debug, Default)]
struct Inner {
    states: HashMap<String, UnitState>,
    /// Unit → id of its queued (pending) job.
    pending: HashMap<String, u64>,
    jobs: Vec<JobRecord>,
    /// Jobs already settled (a replaced job is settled exactly once,
    /// with `canceled` — further settles of it are no-ops).
    settled: HashSet<u64>,
    /// Unit file states for `get_unit_file_state` (bootstrap, spec §8).
    unit_file_states: HashMap<String, String>,
    next_job_id: u64,
    subscribed: bool,
}

impl Inner {
    fn alloc_job(&mut self) -> JobHandle {
        self.next_job_id += 1;
        JobHandle {
            id: self.next_job_id,
        }
    }
}

/// An un-injected unit reads `not-found` + `inactive` — what systemd
/// reports for a unit with no file (spec §10).
fn unknown_unit_state() -> UnitState {
    UnitState {
        load_state: LoadState::NotFound,
        active_state: ActiveState::Inactive,
    }
}

/// In-memory fake (spec §11): manual unit state injection, recorded jobs
/// that settle with an injected result, pushable `JobRemoved` +
/// `PropertiesChanged` signals, and queued-job semantics matching the real
/// `StartUnit("replace")` / `StopUnit("fail")` behavior.
#[derive(Debug)]
pub struct FakeSystemdCtl {
    inner: Mutex<Inner>,
    job_removed_tx: broadcast::Sender<JobRemoved>,
    state_changed_tx: broadcast::Sender<UnitStateChanged>,
    files_changed_tx: broadcast::Sender<()>,
}

impl Default for FakeSystemdCtl {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSystemdCtl {
    pub fn new() -> Self {
        let (job_removed_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (state_changed_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (files_changed_tx, _) = broadcast::channel(SIGNAL_CAPACITY);
        Self {
            inner: Mutex::new(Inner::default()),
            job_removed_tx,
            state_changed_tx,
            files_changed_tx,
        }
    }

    // -- manual unit state injection (test API) -----------------------------

    fn set_active_state(&self, unit: &str, active_state: ActiveState, load_state: LoadState) {
        self.inner.lock().unwrap().states.insert(
            unit.to_owned(),
            UnitState {
                load_state,
                active_state,
            },
        );
    }

    /// Inject `active` (loaded).
    pub fn set_active(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Active, LoadState::Loaded);
    }

    /// Inject `activating` (loaded).
    pub fn set_activating(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Activating, LoadState::Loaded);
    }

    /// Inject `inactive` (loaded).
    pub fn set_inactive(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Inactive, LoadState::Loaded);
    }

    /// Inject `failed` (loaded).
    pub fn set_failed(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Failed, LoadState::Loaded);
    }

    /// Inject `masked`: load state `masked`, reads inactive.
    pub fn set_masked(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Inactive, LoadState::Masked);
    }

    /// Inject `not-found`.
    pub fn set_not_found(&self, unit: &str) {
        self.set_active_state(unit, ActiveState::Inactive, LoadState::NotFound);
    }

    /// Inject an arbitrary unit state (escape hatch for unusual
    /// combinations, e.g. a `stub` load state).
    pub fn set_state(&self, unit: &str, state: UnitState) {
        self.inner
            .lock()
            .unwrap()
            .states
            .insert(unit.to_owned(), state);
    }

    // -- jobs (test API) -----------------------------------------------------

    /// Give a unit a queued job — subsequent `stop_unit` on it is rejected
    /// with [`CtlError::ConflictingJob`], and `start_unit` replaces it. A
    /// previously-pending job is replaced the same way `start_unit` does:
    /// dequeued with result `canceled` (settled exactly once).
    pub fn queue_job(&self, unit: &str) -> JobHandle {
        let mut inner = self.inner.lock().unwrap();
        if let Some(replaced) = inner.pending.remove(unit) {
            inner.settled.insert(replaced);
            let _ = self.job_removed_tx.send(JobRemoved {
                id: replaced,
                unit: unit.to_owned(),
                result: JobResult::Canceled,
            });
        }
        let handle = inner.alloc_job();
        inner.jobs.push(JobRecord {
            handle,
            unit: unit.to_owned(),
            origin: JobOrigin::Queued,
        });
        inner.pending.insert(unit.to_owned(), handle.id);
        handle
    }

    /// Settle a recorded job with an injected result: pushes `JobRemoved`
    /// (spec §7: `JobRemoved` settles the job) and clears the unit's
    /// pending-job entry if it was this job. Unknown or already-settled ids
    /// are ignored — a job settles exactly once.
    pub fn settle_job(&self, id: u64, result: JobResult) {
        let unit = {
            let mut inner = self.inner.lock().unwrap();
            let unit = inner
                .jobs
                .iter()
                .find(|r| r.handle.id == id)
                .map(|r| r.unit.clone());
            let Some(unit) = unit else {
                return;
            };
            if !inner.settled.insert(id) {
                return; // already settled (e.g. replaced) — no double signal.
            }
            if inner.pending.get(&unit) == Some(&id) {
                inner.pending.remove(&unit);
            }
            unit
        };
        let _ = self.job_removed_tx.send(JobRemoved { id, unit, result });
    }

    /// All recorded jobs, in recording order (test introspection).
    pub fn jobs(&self) -> Vec<JobRecord> {
        self.inner.lock().unwrap().jobs.clone()
    }

    /// The unit's queued job id, if any (test introspection).
    pub fn pending_job(&self, unit: &str) -> Option<u64> {
        self.inner.lock().unwrap().pending.get(unit).copied()
    }

    /// Manually push a `PropertiesChanged` edge (watchdog testing, spec
    /// §7): also updates the stored state, like a real signal would.
    pub fn emit_state_changed(&self, unit: &str, active_state: ActiveState) {
        let load_state = self
            .inner
            .lock()
            .unwrap()
            .states
            .get(unit)
            .cloned()
            .map_or(LoadState::NotFound, |state| state.load_state);
        self.set_active_state(unit, active_state.clone(), load_state);
        let _ = self.state_changed_tx.send(UnitStateChanged {
            unit: unit.to_owned(),
            active_state,
        });
    }

    /// Inject a unit file state string (bootstrap step 3, spec §8).
    pub fn set_unit_file_state(&self, unit: &str, state: &str) {
        self.inner
            .lock()
            .unwrap()
            .unit_file_states
            .insert(unit.to_owned(), state.to_owned());
    }

    /// Manually push a unit-file change event (watchdog testing, spec
    /// §7): stands in for `UnitFilesChanged` / a unit's `Reloading(false)`.
    pub fn emit_files_changed(&self) {
        let _ = self.files_changed_tx.send(());
    }

    /// Simulate a bus reconnect (spec §7 re-sync testing): the
    /// per-connection subscription is gone — `subscribe` must run again
    /// before signal reactions re-enable.
    pub fn reset_connection(&self) {
        self.inner.lock().unwrap().subscribed = false;
    }

    /// Whether `subscribe` has been called (test introspection).
    pub fn is_subscribed(&self) -> bool {
        self.inner.lock().unwrap().subscribed
    }
}

impl SystemdCtl for FakeSystemdCtl {
    async fn connect(&self) -> Result<(), CtlError> {
        Ok(())
    }

    async fn subscribe(&self) -> Result<(), CtlError> {
        self.inner.lock().unwrap().subscribed = true;
        Ok(())
    }

    async fn start_unit(&self, unit: &str) -> Result<JobHandle, CtlError> {
        let mut inner = self.inner.lock().unwrap();
        // "replace" mode (man page): a pending job is dequeued with
        // result `canceled` — it never rejects.
        if let Some(replaced) = inner.pending.remove(unit) {
            inner.settled.insert(replaced);
            let _ = self.job_removed_tx.send(JobRemoved {
                id: replaced,
                unit: unit.to_owned(),
                result: JobResult::Canceled,
            });
        }
        let handle = inner.alloc_job();
        inner.jobs.push(JobRecord {
            handle,
            unit: unit.to_owned(),
            origin: JobOrigin::Start,
        });
        inner.pending.insert(unit.to_owned(), handle.id);
        Ok(handle)
    }

    async fn stop_unit(&self, unit: &str) -> Result<JobHandle, CtlError> {
        let mut inner = self.inner.lock().unwrap();
        // "fail" mode (man page): rejected while a job is queued.
        if inner.pending.contains_key(unit) {
            return Err(CtlError::ConflictingJob {
                unit: unit.to_owned(),
            });
        }
        let handle = inner.alloc_job();
        inner.jobs.push(JobRecord {
            handle,
            unit: unit.to_owned(),
            origin: JobOrigin::Stop,
        });
        inner.pending.insert(unit.to_owned(), handle.id);
        Ok(handle)
    }

    fn job_removed(&self) -> broadcast::Receiver<JobRemoved> {
        self.job_removed_tx.subscribe()
    }

    fn unit_state_changed(&self) -> broadcast::Receiver<UnitStateChanged> {
        self.state_changed_tx.subscribe()
    }

    fn unit_files_changed(&self) -> broadcast::Receiver<()> {
        self.files_changed_tx.subscribe()
    }

    async fn get_unit_state(&self, unit: &str) -> Result<UnitState, CtlError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .states
            .get(unit)
            .cloned()
            .unwrap_or_else(unknown_unit_state))
    }

    async fn list_states(&self, units: &[String]) -> Result<Vec<(String, UnitState)>, CtlError> {
        let inner = self.inner.lock().unwrap();
        Ok(units
            .iter()
            .map(|unit| {
                (
                    unit.clone(),
                    inner
                        .states
                        .get(unit)
                        .cloned()
                        .unwrap_or_else(unknown_unit_state),
                )
            })
            .collect())
    }

    async fn get_unit_file_state(&self, unit: &str) -> Result<String, CtlError> {
        let inner = self.inner.lock().unwrap();
        inner
            .unit_file_states
            .get(unit)
            .cloned()
            .ok_or(CtlError::Rejected {
                name: "org.freedesktop.DBus.Error.FileNotFound".into(),
                message: "No such file or directory".into(),
            })
    }

    async fn reload(&self) -> Result<(), CtlError> {
        Ok(())
    }

    async fn enable_unit_files(&self, _units: &[String]) -> Result<(), CtlError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_state(load: LoadState, active: ActiveState) -> UnitState {
        UnitState {
            load_state: load,
            active_state: active,
        }
    }

    #[tokio::test]
    async fn state_reads_reflect_injections() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_activating("b.service");
        fake.set_inactive("c.service");
        fake.set_failed("d.service");
        fake.set_masked("e.service");
        fake.set_not_found("f.service");

        let loaded = LoadState::Loaded;
        assert_eq!(
            fake.get_unit_state("a.service").await.unwrap(),
            unit_state(loaded.clone(), ActiveState::Active)
        );
        assert_eq!(
            fake.get_unit_state("b.service").await.unwrap(),
            unit_state(loaded.clone(), ActiveState::Activating)
        );
        assert_eq!(
            fake.get_unit_state("c.service").await.unwrap(),
            unit_state(loaded.clone(), ActiveState::Inactive)
        );
        assert_eq!(
            fake.get_unit_state("d.service").await.unwrap(),
            unit_state(loaded.clone(), ActiveState::Failed)
        );
        assert_eq!(
            fake.get_unit_state("e.service").await.unwrap(),
            unit_state(LoadState::Masked, ActiveState::Inactive)
        );
        assert_eq!(
            fake.get_unit_state("f.service").await.unwrap(),
            unit_state(LoadState::NotFound, ActiveState::Inactive)
        );
        // Never injected: reads not-found + inactive.
        assert_eq!(
            fake.get_unit_state("g.service").await.unwrap(),
            unit_state(LoadState::NotFound, ActiveState::Inactive)
        );
    }

    #[tokio::test]
    async fn list_states_keeps_input_order_and_covers_unknowns() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("zeta.service");
        fake.set_active("alpha.service");

        let units: Vec<String> = ["middle.service", "alpha.service", "zeta.service"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let states = fake.list_states(&units).await.unwrap();
        assert_eq!(
            states
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["middle.service", "alpha.service", "zeta.service"]
        );
        assert_eq!(
            states[1].1,
            unit_state(LoadState::Loaded, ActiveState::Active)
        );
        assert_eq!(states[0].1.load_state, LoadState::NotFound);
    }

    #[tokio::test]
    async fn jobs_settle_with_the_injected_result() {
        let fake = FakeSystemdCtl::new();
        let mut rx = fake.job_removed();
        let handle = fake.start_unit("foo.service").await.unwrap();

        fake.settle_job(handle.id, JobResult::Done);
        let removed = rx.recv().await.unwrap();
        assert_eq!(
            removed,
            JobRemoved {
                id: handle.id,
                unit: "foo.service".into(),
                result: JobResult::Done
            }
        );

        // A job settles exactly once: a second settle is a no-op.
        fake.settle_job(handle.id, JobResult::Failed);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_result_variant_carries_through_settle() {
        let fake = FakeSystemdCtl::new();
        let mut rx = fake.job_removed(); // subscribe before any push

        let results = [
            JobResult::Done,
            JobResult::Skipped,
            JobResult::Failed,
            JobResult::Canceled,
            JobResult::Timeout,
            JobResult::Dependency,
        ];
        let units: Vec<String> = (0..6).map(|i| format!("u{i}.service")).collect();
        let mut handles = Vec::new();
        for unit in &units {
            handles.push(fake.start_unit(unit).await.unwrap());
        }
        for ((_, handle), result) in units.iter().zip(handles.iter()).zip(results.iter()) {
            fake.settle_job(handle.id, result.clone());
        }
        for ((unit, handle), result) in units.iter().zip(handles.iter()).zip(results.iter()) {
            let removed = rx.recv().await.unwrap();
            assert_eq!(
                removed,
                JobRemoved {
                    id: handle.id,
                    unit: unit.clone(),
                    result: result.clone()
                }
            );
        }
    }

    #[tokio::test]
    async fn stop_is_rejected_while_a_job_is_pending() {
        let fake = FakeSystemdCtl::new();

        // No pending job: stop succeeds and records the job.
        let handle = fake.stop_unit("foo.service").await.unwrap();
        assert_eq!(fake.pending_job("foo.service"), Some(handle.id));

        // A pending job (whatever its origin) makes stop reject.
        let err = fake.stop_unit("foo.service").await.unwrap_err();
        assert_eq!(
            err,
            CtlError::ConflictingJob {
                unit: "foo.service".into()
            }
        );

        // JobRemoved settles the job; the pending entry clears; stop works.
        fake.settle_job(handle.id, JobResult::Done);
        assert_eq!(fake.pending_job("foo.service"), None);
        assert!(fake.stop_unit("foo.service").await.is_ok());
    }

    #[tokio::test]
    async fn start_replaces_a_pending_job_with_canceled() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("foo.service");

        let mut rx = fake.job_removed();
        let queued = fake.queue_job("foo.service");

        // "replace" mode: start never rejects; the pending job is
        // dequeued with result `canceled`.
        let handle = fake.start_unit("foo.service").await.unwrap();
        let removed = rx.recv().await.unwrap();
        assert_eq!(
            removed,
            JobRemoved {
                id: queued.id,
                unit: "foo.service".into(),
                result: JobResult::Canceled
            }
        );
        assert_eq!(fake.pending_job("foo.service"), Some(handle.id));

        // The replaced job already settled (with `canceled`): settling it
        // again emits nothing.
        fake.settle_job(queued.id, JobResult::Done);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn state_changed_signals_are_observable() {
        let fake = FakeSystemdCtl::new();
        let mut rx = fake.unit_state_changed();

        fake.emit_state_changed("foo.service", ActiveState::Activating);
        let edge = rx.recv().await.unwrap();
        assert_eq!(
            edge,
            UnitStateChanged {
                unit: "foo.service".into(),
                active_state: ActiveState::Activating
            }
        );
        // The stored state reflects the edge too.
        assert_eq!(
            fake.get_unit_state("foo.service")
                .await
                .unwrap()
                .active_state,
            ActiveState::Activating
        );
    }

    #[tokio::test]
    async fn signals_have_no_replay() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("foo.service");
        let queued = fake.queue_job("foo.service");
        // Push before anyone subscribes.
        fake.settle_job(queued.id, JobResult::Done);

        let mut rx = fake.job_removed();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscribe_is_recorded() {
        let fake = FakeSystemdCtl::new();
        assert!(!fake.is_subscribed());
        fake.subscribe().await.unwrap();
        assert!(fake.is_subscribed());
    }

    /// Compile-time proof that the seam is impl-agnostic: every trait
    /// method callable through a generic bound (the real backend will fit
    /// the same shape, spec §11).
    async fn seam_smoke<T: SystemdCtl>(ctl: &T) {
        drop(ctl.job_removed());
        drop(ctl.unit_state_changed());
        ctl.connect().await.unwrap();
        ctl.subscribe().await.unwrap();
        let h1 = ctl.start_unit("a.service").await.unwrap();
        let h2 = ctl.stop_unit("b.service").await.unwrap();
        assert_ne!(h1.id, h2.id);
        let one = ctl.get_unit_state("a.service").await.unwrap();
        let many = ctl
            .list_states(&["a.service".into(), "b.service".into()])
            .await
            .unwrap();
        assert_eq!(many.len(), 2);
        assert!(matches!(
            one.load_state,
            LoadState::NotFound | LoadState::Loaded
        ));
        ctl.reload().await.unwrap();
        ctl.enable_unit_files(&["ussd.service".into()])
            .await
            .unwrap();
        let _ = ctl.get_unit_file_state("a.service").await;
    }

    #[tokio::test]
    async fn seam_smoke_runs_against_the_fake() {
        let fake = FakeSystemdCtl::new();
        seam_smoke(&fake).await;
    }

    #[tokio::test]
    async fn unit_file_state_injection_and_unknown_rejection() {
        let fake = FakeSystemdCtl::new();
        fake.set_unit_file_state("ussd.service", "enabled");
        assert_eq!(
            fake.get_unit_file_state("ussd.service").await.unwrap(),
            "enabled"
        );
        let err = fake.get_unit_file_state("nope.service").await.unwrap_err();
        assert!(matches!(
            err,
            CtlError::Rejected {
                ref name,
                ..
            } if name.ends_with("FileNotFound")
        ));
    }

    #[tokio::test]
    async fn queue_job_replaces_a_pending_job_with_canceled() {
        let fake = FakeSystemdCtl::new();
        let mut rx = fake.job_removed();
        let first = fake.queue_job("u.service");
        let second = fake.queue_job("u.service");
        let removed = rx.recv().await.unwrap();
        assert_eq!(
            removed,
            JobRemoved {
                id: first.id,
                unit: "u.service".into(),
                result: JobResult::Canceled
            }
        );
        assert_eq!(fake.pending_job("u.service"), Some(second.id));
        // The replaced job already settled — settling it again emits nothing.
        fake.settle_job(first.id, JobResult::Done);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn job_result_wire_strings() {
        assert_eq!(JobResult::Done.to_string(), "done");
        assert_eq!(JobResult::Skipped.to_string(), "skipped");
        assert_eq!(JobResult::Canceled.to_string(), "canceled");
        assert_eq!(JobResult::Timeout.to_string(), "timeout");
        assert_eq!(JobResult::Dependency.to_string(), "dependency");
        assert_eq!(JobResult::Failed.to_string(), "failed");
        assert_eq!(JobResult::Other("weird".into()).to_string(), "weird");
        // Success set per spec §3.
        assert!(JobResult::Done.is_success());
        assert!(JobResult::Skipped.is_success());
        for r in [
            JobResult::Failed,
            JobResult::Canceled,
            JobResult::Timeout,
            JobResult::Dependency,
            JobResult::Other("x".into()),
        ] {
            assert!(!r.is_success(), "{r:?} must not be success");
        }
    }
}
