//! ussd's core command handlers: `status`, `add`, `remove` (spec §4.2/§4.3).
//!
//! Written against the [`SystemdCtl`] seam (spec §11) — the real backend and
//! the fake both fit. This module is the control plane minus transport:
//! the socket layer (a later slice) parses protocol requests, calls these,
//! and translates [`Error`]s back to protocol responses via
//! `Error::code()` / `Display`.
//!
//! All handlers take the normalized view: inputs are re-validated here
//! (defense in depth — the CLI validates client-side too, §4.4), and
//! storage/output use the full `*.service` form everywhere (§10).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::error::{Error, OpError, OpVerb};
use crate::protocol::{GroupStatus, MemberStatus, StatusResult};
use crate::state::{Groups, StateError};
use crate::systemdctl::{
    ActiveState, CtlError, JobHandle, JobRemoved, JobResult, LoadState, SystemdCtl,
};

/// A `JobRemoved` receiver subscribed BEFORE the operation it waits for was
/// issued — the race-free subscribe-then-call pattern (spec §3; the
/// `org.freedesktop.systemd1` man page). Signals have no replay, so a
/// receiver created after the D-Bus reply would miss a `JobRemoved` that
/// fired in the window between the reply and the subscription.
type JobRx = Receiver<JobRemoved>;

/// Persistent state the core mutates and persists (spec §5).
pub struct State {
    pub groups: Groups,
    path: PathBuf,
}

impl State {
    /// Load the state file (missing file = empty state, §5).
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, StateError> {
        let path = path.into();
        Ok(Self {
            groups: Groups::load(&path)?,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomic persist (spec §5). Failures map to `internal` (exit 8) —
    /// §4.4 has no dedicated class for state IO mid-command. On a persist
    /// failure the in-memory `Groups` is ahead of the file: `add` self-heals
    /// on retry (the no-op re-add re-persists); a `remove` divergence clears
    /// when ussd next reloads the file. The spec defines no persist-failure
    /// semantics beyond the exit 8.
    pub fn persist(&self) -> Result<(), Error> {
        self.groups.save(&self.path).map_err(|e| Error::Internal {
            message: e.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `status` (spec §4.3/§6): point-in-time snapshot. Groups alphabetical,
/// members in add order, `active` = live `ActiveState == "active"`.
/// `only` = the single-group variant; unknown group → exit 2.
pub async fn status<C: SystemdCtl>(
    ctl: &C,
    groups: &Groups,
    only: Option<&str>,
) -> Result<StatusResult, Error> {
    let selected: Vec<(String, Vec<String>)> = match only {
        Some(g) => match groups.members(g) {
            Some(members) => vec![(g.to_owned(), members.to_vec())],
            None => {
                return Err(Error::UnknownGroup {
                    group: g.to_owned(),
                });
            }
        },
        None => groups
            .group_names()
            .into_iter()
            .map(|g| (g.to_owned(), groups.members(g).unwrap().to_vec()))
            .collect(),
    };

    let units: Vec<String> = selected
        .iter()
        .flat_map(|(_, members)| members.iter().cloned())
        .collect();
    let states = ctl.list_states(&units).await.map_err(map_query_error)?;
    let active: HashMap<String, bool> = states
        .iter()
        .map(|(name, s)| (name.clone(), s.active_state == ActiveState::Active))
        .collect();

    Ok(StatusResult {
        groups: selected
            .into_iter()
            .map(|(name, members)| GroupStatus {
                name,
                members: members
                    .into_iter()
                    .map(|name| MemberStatus {
                        active: active.get(&name).copied().unwrap_or(false),
                        name,
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// `add` (spec §4.2): normalize + validate; the group is created if new;
/// a service in another group is rejected (exit 4); the unit must be
/// loadable (exit 5 class); already a member of the same group is a no-op
/// success. Adding never changes active state; persist after success.
pub async fn add<C: SystemdCtl>(
    ctl: &C,
    group_input: &str,
    service_input: &str,
    state: &mut State,
) -> Result<(), Error> {
    crate::names::validate_group(group_input)?;
    let service = crate::names::normalize_service(service_input)?;
    let group = group_input.to_owned();

    // Cross-group invariant before the loadability check (§4.2 order).
    if let Some(other) = state.groups.group_of(&service) {
        if other == group {
            return Ok(()); // no-op success — already a member here.
        }
        return Err(Error::ServiceInOtherGroup {
            service,
            other: other.to_owned(),
        });
    }

    // Loadability check (§10): loaded/stub ok; not-found / masked → exit 5.
    let st = ctl
        .get_unit_state(&service)
        .await
        .map_err(map_query_error)?;
    match st.load_state {
        LoadState::Loaded | LoadState::Stub => {}
        LoadState::NotFound => return Err(Error::ServiceNotFound { service }),
        LoadState::Masked => return Err(Error::ServiceMasked { service }),
        // Unusual load states (e.g. `error`): not loadable — the bare
        // not-found template is the §4.4 fit (exit 5 class either way);
        // the load state itself is diagnostic detail the spec does not
        // render.
        LoadState::Other(_) => return Err(Error::ServiceNotFound { service }),
    }

    state.groups.add_member(&group, &service);
    state.persist()?;
    Ok(())
}

/// `remove` (spec §4.2): unknown group (exit 2), non-member (exit 3). If
/// the member is active it is stopped first — persist only after the stop
/// succeeds; a failed stop leaves the member in the group (exit 6, nothing
/// changed). The group is deleted when the last member goes.
pub async fn remove<C: SystemdCtl>(
    ctl: &C,
    group_input: &str,
    service_input: &str,
    state: &mut State,
) -> Result<(), Error> {
    crate::names::validate_group(group_input)?;
    let service = crate::names::normalize_service(service_input)?;
    let group = group_input.to_owned();

    if !state.groups.has_group(&group) {
        return Err(Error::UnknownGroup { group });
    }
    let is_member = state
        .groups
        .members(&group)
        .unwrap()
        .iter()
        .any(|m| m == &service);
    if !is_member {
        return Err(Error::NotAMember { group, service });
    }

    // Active member: stop first, per the §4.2 conflicting-job rule.
    let st = ctl
        .get_unit_state(&service)
        .await
        .map_err(map_query_error)?;
    if st.active_state == ActiveState::Active {
        stop_unit_with_rule(ctl, &service).await?;
    }

    state.groups.remove_member(&group, &service);
    state.persist()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// §4.2 stop machinery (shared with the later `start` switch)
// ---------------------------------------------------------------------------

/// `StopUnit(unit, "fail")` confirmed via `JobRemoved` (spec §3), with the
/// §4.2 conflicting-job rule. The `JobRemoved` receiver is subscribed
/// BEFORE the first `stop_unit` call (subscribe-then-call, §3) and reused
/// for every wait in this function — a later subscription could miss the
/// signal for the just-issued job.
///
/// - stop rejected by a queued job → wait for that job's `JobRemoved`
///   (filtered by unit), re-read `ActiveState`:
///   - not active → the stop phase is satisfied;
///   - still active (an in-flight start completed) → re-issue `StopUnit`
///     once;
///   - rejected again → `failed to stop <service>: conflicting job`
///     (exit 6), state unchanged.
/// - any other rejection → `op-failed` (exit 6).
/// - a bad job result (`failed`/`canceled`/…) → `op-failed` (exit 6).
pub async fn stop_unit_with_rule<C: SystemdCtl>(ctl: &C, service: &str) -> Result<(), Error> {
    let mut rx = ctl.job_removed();
    match ctl.stop_unit(service).await {
        Ok(job) => job_outcome(&mut rx, job, OpVerb::Stop, service).await,
        Err(CtlError::ConflictingJob { .. }) => {
            wait_job_removed_for_unit(&mut rx, service).await?;
            let st = ctl.get_unit_state(service).await.map_err(map_query_error)?;
            if st.active_state != ActiveState::Active {
                return Ok(()); // the stop phase is satisfied.
            }
            // Still active: re-issue once (§4.2), on the same receiver.
            match ctl.stop_unit(service).await {
                Ok(job) => job_outcome(&mut rx, job, OpVerb::Stop, service).await,
                Err(CtlError::ConflictingJob { .. }) => {
                    Err(Error::OpFailed(OpError::ConflictingJob {
                        service: service.to_owned(),
                    }))
                }
                Err(e) => Err(map_op_error(e, OpVerb::Stop, service)),
            }
        }
        Err(e) => Err(map_op_error(e, OpVerb::Stop, service)),
    }
}

/// Wait for a job's `JobRemoved` and map its result (§7: success =
/// `done`/`skipped`; anything else fails the waiting command).
async fn job_outcome(
    rx: &mut JobRx,
    job: JobHandle,
    op: OpVerb,
    service: &str,
) -> Result<(), Error> {
    let result = wait_job_removed(rx, job.id).await?;
    if result.is_success() {
        Ok(())
    } else {
        Err(Error::OpFailed(OpError::JobResult {
            op,
            service: service.to_owned(),
            result: result.to_string(),
        }))
    }
}

/// Wait for the `JobRemoved` of a specific job id (other jobs' signals are
/// ignored — the command queue is sequential, §6). `Lagged` is a failure,
/// not a retry: the awaited signal may be in the lost window, and §3's
/// re-sync (re-read live state) cannot recover a job's RESULT (a job-state
/// query is not in the seam — slice 7 can add one if this proves real).
async fn wait_job_removed(rx: &mut JobRx, id: u64) -> Result<JobResult, Error> {
    loop {
        let ev = recv_job(rx).await?;
        if ev.id == id {
            return Ok(ev.result);
        }
    }
}

/// Wait for the next `JobRemoved` of a specific unit — the §4.2
/// conflicting-job rule does not know the queued job's id.
async fn wait_job_removed_for_unit(rx: &mut JobRx, unit: &str) -> Result<JobResult, Error> {
    loop {
        let ev = recv_job(rx).await?;
        if ev.unit == unit {
            return Ok(ev.result);
        }
    }
}

/// One `JobRemoved` event, with the two `recv` failures told apart:
/// `Lagged` (missed events) ≠ `Closed` (the stream ended).
async fn recv_job(rx: &mut JobRx) -> Result<JobRemoved, Error> {
    rx.recv().await.map_err(|e| match e {
        RecvError::Lagged(n) => Error::Internal {
            message: format!("job stream lagged: {n} events lost — command aborted"),
        },
        RecvError::Closed => Error::Internal {
            message: "job stream closed".into(),
        },
    })
}

// ---------------------------------------------------------------------------
// CtlError → Error mapping (§4.4)
// ---------------------------------------------------------------------------

/// A start/stop operation was rejected (exit 6 row).
fn map_op_error(e: CtlError, op: OpVerb, service: &str) -> Error {
    match e {
        CtlError::Rejected { name, message } => Error::OpFailed(OpError::Rejected {
            op,
            service: service.to_owned(),
            dbus_name: name,
            dbus_message: message,
        }),
        CtlError::ConflictingJob { unit } => {
            Error::OpFailed(OpError::ConflictingJob { service: unit })
        }
        CtlError::ManagerAbsent => Error::Internal {
            message: "user manager connection unavailable".into(),
        },
    }
}

/// A state query failed (not a start/stop operation): no §4.4 row fits —
/// `daemon internal` (exit 8) with the reason in the message.
fn map_query_error(e: CtlError) -> Error {
    match e {
        CtlError::Rejected { name, message } => Error::Internal {
            message: format!("systemd rejected the query: {name} ({message})"),
        },
        CtlError::ConflictingJob { unit } => Error::Internal {
            message: format!("unexpected conflicting job on {unit} during query"),
        },
        CtlError::ManagerAbsent => Error::Internal {
            message: "user manager connection unavailable".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemdctl::{FakeSystemdCtl, JobOrigin, UnitState};
    use std::fs;
    use std::path::PathBuf;

    /// A unique temp dir per test, removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("uss-core-test-{}-{}", std::process::id(), tag));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn state(&self) -> State {
            State::load(self.0.join("uss").join("groups.json")).unwrap()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // -- status -------------------------------------------------------------

    #[tokio::test]
    async fn status_is_a_point_in_time_snapshot() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        fake.set_failed("openvpn.service");
        fake.set_activating("foo.service");

        let mut groups = Groups::default();
        groups.add_member("vpn", "openvpn.service");
        groups.add_member("vpn", "wireguard.service");
        groups.add_member("dev", "foo.service");

        let result = status(&fake, &groups, None).await.unwrap();
        // Groups alphabetical; members in add order; active is the LIVE
        // ActiveState (failed/activating print bare — active=false).
        assert_eq!(
            result
                .groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            vec!["dev", "vpn"]
        );
        let vpn = &result.groups[1];
        assert_eq!(
            vpn.members
                .iter()
                .map(|m| (m.name.as_str(), m.active))
                .collect::<Vec<_>>(),
            vec![("openvpn.service", false), ("wireguard.service", true)]
        );
        assert!(!result.groups[0].members[0].active);

        // Live: a state change is visible to the next query (re-sync,
        // don't trust history).
        fake.set_inactive("wireguard.service");
        let result = status(&fake, &groups, None).await.unwrap();
        assert!(!result.groups[1].members[1].active);
    }

    #[tokio::test]
    async fn status_single_group_and_unknown_group() {
        let fake = FakeSystemdCtl::new();
        let mut groups = Groups::default();
        groups.add_member("vpn", "wireguard.service");

        let result = status(&fake, &groups, Some("vpn")).await.unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].name, "vpn");

        let err = status(&fake, &groups, Some("dev")).await.unwrap_err();
        assert_eq!(
            err,
            Error::UnknownGroup {
                group: "dev".into()
            }
        );
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn status_with_no_groups_is_empty() {
        let fake = FakeSystemdCtl::new();
        let groups = Groups::default();
        let result = status(&fake, &groups, None).await.unwrap();
        assert!(result.groups.is_empty());
    }

    // -- add ----------------------------------------------------------------

    #[tokio::test]
    async fn add_creates_group_normalizes_and_persists() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("openvpn.service"); // loaded (the default load state)
        let tmp = Tmp::new("add-create");
        let mut state = tmp.state();

        add(&fake, "vpn", "openvpn", &mut state).await.unwrap(); // bare name

        assert!(state.groups.has_group("vpn"));
        assert_eq!(
            state.groups.members("vpn").unwrap(),
            vec!["openvpn.service"]
        );
        // Persisted with schema v1 and the normalized name.
        let reloaded = State::load(tmp.0.join("uss").join("groups.json")).unwrap();
        assert_eq!(
            reloaded.groups.members("vpn").unwrap(),
            vec!["openvpn.service"]
        );
    }

    #[tokio::test]
    async fn add_no_op_for_existing_member_of_same_group() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("openvpn.service");
        let tmp = Tmp::new("add-noop");
        let mut state = tmp.state();

        add(&fake, "vpn", "openvpn.service", &mut state)
            .await
            .unwrap();
        add(&fake, "vpn", "openvpn.service", &mut state)
            .await
            .unwrap(); // no-op

        assert_eq!(state.groups.members("vpn").unwrap().len(), 1); // no duplicate
    }

    #[tokio::test]
    async fn add_rejects_service_in_another_group() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("foo.service");
        let tmp = Tmp::new("add-conflict");
        let mut state = tmp.state();
        state.groups.add_member("dev", "foo.service");

        let err = add(&fake, "vpn", "foo.service", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::ServiceInOtherGroup {
                service: "foo.service".into(),
                other: "dev".into()
            }
        );
        assert_eq!(err.exit_code(), 4);
        // Nothing changed: the service is still only in `dev`.
        assert_eq!(state.groups.group_of("foo.service"), Some("dev"));
        assert!(!state.groups.has_group("vpn"));
    }

    #[tokio::test]
    async fn add_rejects_unloadable_units() {
        let fake = FakeSystemdCtl::new();
        fake.set_not_found("nope.service");
        fake.set_masked("masked.service");
        let tmp = Tmp::new("add-unloadable");
        let mut state = tmp.state();

        let err = add(&fake, "vpn", "nope.service", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::ServiceNotFound {
                service: "nope.service".into()
            }
        );
        assert_eq!(err.exit_code(), 5);

        let err = add(&fake, "vpn", "masked.service", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::ServiceMasked {
                service: "masked.service".into()
            }
        );
        assert_eq!(err.exit_code(), 5);

        // A never-injected unit reads not-found too.
        let err = add(&fake, "vpn", "ghost.service", &mut state)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::ServiceNotFound { .. }));

        // `stub` units are loadable.
        fake.set_state(
            "stub.service",
            UnitState {
                load_state: LoadState::Stub,
                active_state: ActiveState::Inactive,
            },
        );
        add(&fake, "vpn", "stub.service", &mut state).await.unwrap();
        assert!(state.groups.has_group("vpn"));
    }

    #[tokio::test]
    async fn add_rejects_invalid_names() {
        let fake = FakeSystemdCtl::new();
        let tmp = Tmp::new("add-names");
        let mut state = tmp.state();

        let err = add(&fake, "vpn", "foo.target", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::InvalidServiceName {
                input: "foo.target".into()
            }
        );
        assert_eq!(err.exit_code(), 1);

        let err = add(&fake, "a/b", "foo", &mut state).await.unwrap_err();
        assert_eq!(
            err,
            Error::InvalidGroupName {
                input: "a/b".into()
            }
        );
        assert_eq!(err.exit_code(), 1);
        assert!(!state.groups.has_group("a/b"));
    }

    // -- remove -------------------------------------------------------------

    #[tokio::test]
    async fn remove_inactive_member_and_last_member_deletes_group() {
        let fake = FakeSystemdCtl::new();
        fake.set_inactive("openvpn.service");
        let tmp = Tmp::new("remove-basic");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "openvpn.service");
        state.groups.add_member("vpn", "wireguard.service");

        remove(&fake, "vpn", "openvpn.service", &mut state)
            .await
            .unwrap();
        assert_eq!(
            state.groups.members("vpn").unwrap(),
            vec!["wireguard.service"]
        );

        // Last member: the group is deleted and the file reflects it.
        fake.set_inactive("wireguard.service");
        remove(&fake, "vpn", "wireguard", &mut state).await.unwrap(); // bare
        assert!(!state.groups.has_group("vpn"));
        let reloaded = State::load(tmp.0.join("uss").join("groups.json")).unwrap();
        assert!(reloaded.groups.is_empty());
    }

    #[tokio::test]
    async fn remove_preconditions_unknown_group_and_non_member() {
        let fake = FakeSystemdCtl::new();
        fake.set_inactive("foo.service");
        let tmp = Tmp::new("remove-precond");
        let mut state = tmp.state();
        state.groups.add_member("dev", "foo.service");

        let err = remove(&fake, "vpn", "foo.service", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::UnknownGroup {
                group: "vpn".into()
            }
        );
        assert_eq!(err.exit_code(), 2);

        let err = remove(&fake, "dev", "other.service", &mut state)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::NotAMember {
                group: "dev".into(),
                service: "other.service".into()
            }
        );
        assert_eq!(err.exit_code(), 3);

        // State unchanged.
        assert_eq!(state.groups.members("dev").unwrap(), vec!["foo.service"]);
    }

    #[tokio::test]
    async fn remove_active_member_stops_then_persists() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        let tmp = Tmp::new("remove-active");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "wireguard.service");

        // The handler is an async fn: nothing runs until polled. Poll it
        // (no-op waker) and react between polls — after the first poll the
        // core has issued StopUnit and suspends on the job's JobRemoved.
        // The pinned future holds `&mut state` until dropped, so the
        // assertions run after the block.
        let result = {
            let mut fut = Box::pin(remove(&fake, "vpn", "wireguard.service", &mut state));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            loop {
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        if let Some(job) = fake.pending_job("wireguard.service") {
                            fake.settle_job(job, JobResult::Done);
                        }
                    }
                }
            }
        };
        result.unwrap();
        // Pin the stop: exactly one Stop-origin job for the unit was
        // issued — the test would otherwise pass vacuously if the
        // handler skipped the stop entirely.
        assert_eq!(
            fake.jobs()
                .iter()
                .filter(|j| j.origin == JobOrigin::Stop && j.unit == "wireguard.service")
                .count(),
            1
        );
        assert!(!state.groups.has_group("vpn"));
        let reloaded = State::load(tmp.0.join("uss").join("groups.json")).unwrap();
        assert!(reloaded.groups.is_empty());
    }

    #[tokio::test]
    async fn remove_failed_stop_leaves_member_and_state_unchanged() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        let tmp = Tmp::new("remove-failed-stop");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "wireguard.service");

        let result = {
            let mut fut = Box::pin(remove(&fake, "vpn", "wireguard.service", &mut state));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            loop {
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        if let Some(job) = fake.pending_job("wireguard.service") {
                            fake.settle_job(job, JobResult::Failed);
                        }
                    }
                }
            }
        };
        let err = result.unwrap_err();

        // exit 6, op-failed with the job result — and nothing changed.
        assert!(matches!(
            &err,
            Error::OpFailed(OpError::JobResult {
                op: OpVerb::Stop,
                result,
                ..
            }) if *result == "failed"
        ));
        assert_eq!(err.exit_code(), 6);
        assert_eq!(
            state.groups.members("vpn").unwrap(),
            vec!["wireguard.service"]
        );
    }

    #[tokio::test]
    async fn remove_conflicting_job_not_active_after_settle_succeeds() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        let queued = fake.queue_job("wireguard.service");
        let tmp = Tmp::new("remove-conflict-satisfied");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "wireguard.service");

        // The core's stop was rejected (queued job) and it waits for the
        // unit's JobRemoved. Settle it and leave the unit not active: the
        // stop phase is satisfied.
        let result = {
            let mut fut = Box::pin(remove(&fake, "vpn", "wireguard.service", &mut state));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            loop {
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        if fake.pending_job("wireguard.service") == Some(queued.id) {
                            fake.settle_job(queued.id, JobResult::Done);
                            fake.set_inactive("wireguard.service");
                        }
                    }
                }
            }
        };
        result.unwrap();
        assert!(!state.groups.has_group("vpn"));
    }

    #[tokio::test]
    async fn remove_conflicting_job_still_active_reissues_once() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        let queued = fake.queue_job("wireguard.service");
        let tmp = Tmp::new("remove-conflict-reissue");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "wireguard.service");

        let result = {
            let mut fut = Box::pin(remove(&fake, "vpn", "wireguard.service", &mut state));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut reissue_settled = false;
            let mut polls = 0u32;
            loop {
                polls += 1;
                assert!(
                    polls <= 50,
                    "poll cap exceeded — the handler looped instead of terminating"
                );
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        let pending = fake.pending_job("wireguard.service");
                        if !reissue_settled && pending == Some(queued.id) {
                            // Settle the queued job but keep the unit active:
                            // an in-flight start completed → the core re-issues
                            // StopUnit once.
                            fake.settle_job(queued.id, JobResult::Done);
                        } else if pending != Some(queued.id) && pending.is_some() {
                            // The re-issued stop job: settle it successfully.
                            fake.settle_job(pending.unwrap(), JobResult::Done);
                            reissue_settled = true;
                        }
                    }
                }
            }
        };
        result.unwrap();
        // Exactly one re-issued stop job (the first stop was rejected,
        // not queued): the rule re-issues once and no more.
        assert_eq!(
            fake.jobs()
                .iter()
                .filter(|j| j.origin == JobOrigin::Stop && j.unit == "wireguard.service")
                .count(),
            1
        );
        assert!(!state.groups.has_group("vpn"));
    }

    #[tokio::test]
    async fn remove_conflicting_job_rejected_twice_aborts_exit_6() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("wireguard.service");
        let queued = fake.queue_job("wireguard.service");
        let tmp = Tmp::new("remove-conflict-exhausted");
        let mut state = tmp.state();
        state.groups.add_member("vpn", "wireguard.service");

        let result = {
            let mut fut = Box::pin(remove(&fake, "vpn", "wireguard.service", &mut state));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut first_done = false;
            let mut polls = 0u32;
            loop {
                polls += 1;
                assert!(
                    polls <= 50,
                    "poll cap exceeded — the handler looped instead of aborting"
                );
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        let pending = fake.pending_job("wireguard.service");
                        if !first_done && pending == Some(queued.id) {
                            // Settle the queued job (unit stays active →
                            // re-issue), and queue ANOTHER job so the re-issue
                            // is rejected again → the rule exhausts.
                            fake.settle_job(queued.id, JobResult::Done);
                            fake.queue_job("wireguard.service");
                            first_done = true;
                        }
                    }
                }
            }
        };
        let err = result.unwrap_err();
        assert_eq!(
            err,
            Error::OpFailed(OpError::ConflictingJob {
                service: "wireguard.service".into()
            })
        );
        assert_eq!(err.exit_code(), 6);
        // Nothing was ever queued as a Stop job: both stop calls were
        // rejected (no re-issue job, no wait beyond the rule).
        assert_eq!(
            fake.jobs()
                .iter()
                .filter(|j| j.origin == JobOrigin::Stop)
                .count(),
            0
        );
        // Nothing changed: the member is still in the group.
        assert_eq!(
            state.groups.members("vpn").unwrap(),
            vec!["wireguard.service"]
        );
    }
}
