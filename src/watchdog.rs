//! ussd's watchdog (spec §7): out-of-band start detection on group members.
//!
//! The daemon (the later lifecycle slice) pumps the seam's signals and
//! hands this module:
//! - [`Watchdog::resync`] — on startup and after every bus reconnect,
//!   BEFORE signal reactions re-enable (signals are not replayed);
//! - [`Watchdog::push_edge`] + [`Watchdog::process_next`] — member
//!   `ActiveState` edges, processed serially in push order: the
//!   convergence property (spec §7) depends on it;
//! - [`Watchdog::revalidate_unit_files`] — on `UnitFilesChanged` /
//!   `Reloading(false)`.
//!
//! A reaction reads the LIVE state at execution time: a member that is no
//! longer `active` when its edge runs has a stale edge — dropped. A stop
//! failure (an ignores-stop member, or the §4.2 conflicting-job rule
//! exhausted) is logged to the journal (stderr — the unit's journal sink)
//! and the edge is consumed: the group stays double-active until the next
//! start edge re-asserts the invariant. No automatic membership changes
//! anywhere in this module.

use std::collections::{HashMap, VecDeque};

use crate::core::{map_query_error, stop_unit_with_rule};
use crate::error::Error;
use crate::jobs::JobBook;
use crate::state::Groups;
use crate::systemdctl::{ActiveState, SystemdCtl, UnitState, UnitStateChanged};

/// Queued member edges, processed serially in push order.
pub struct Watchdog {
    pending: VecDeque<UnitStateChanged>,
}

/// The outcome of processing one queued edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// The edge is not a start edge (`inactive`/`failed`/…): no reaction —
    /// zero active is legal (spec §7).
    NoStartEdge,
    /// The unit is not a member of any group: ignored.
    NotAMember,
    /// An in-flight ussd job behind the edge (the switch ticket's
    /// bookkeeping): ussd's own doing, suppressed.
    Suppressed,
    /// The member is no longer `active` at execution time: the edge is
    /// stale and dropped (spec §7).
    Stale,
    /// The member is active and no other member is: the invariant already
    /// holds; nothing to stop.
    NoConflicts,
    /// The other active members were stopped, `JobRemoved`-confirmed (the
    /// §4.2 conflicting-job rule applies to these stops). Failures were
    /// logged to the journal and the edge consumed: the group stays
    /// double-active until the next start edge re-asserts.
    Stops {
        stopped: Vec<String>,
        failed: Vec<String>,
    },
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

impl Watchdog {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }

    /// Queued edges not yet processed (test introspection).
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Re-sync (spec §7): after connecting or reconnecting — re-subscribe
    /// (per-connection, spec §3) and re-read all members' live
    /// `ActiveState` before re-enabling signal reactions (signals are not
    /// replayed). Returns the fresh snapshot.
    pub async fn resync<C: SystemdCtl>(
        ctl: &C,
        groups: &Groups,
    ) -> Result<Vec<(String, UnitState)>, Error> {
        ctl.subscribe().await.map_err(map_query_error)?;
        ctl.list_states(&all_member_units(groups))
            .await
            .map_err(map_query_error)
    }

    /// The `UnitFilesChanged` / `Reloading(false)` reaction (spec §7):
    /// re-validate member unit files (newly masked/removed). No automatic
    /// membership changes — a masked or vanished unit simply reads
    /// inactive and surfaces a structured error at the next `start`/`add`.
    /// Returns the fresh per-member state.
    pub async fn revalidate_unit_files<C: SystemdCtl>(
        ctl: &C,
        groups: &Groups,
    ) -> Result<Vec<(String, UnitState)>, Error> {
        ctl.list_states(&all_member_units(groups))
            .await
            .map_err(map_query_error)
    }

    /// Queue a member `ActiveState` edge (the daemon's signal pump calls
    /// this for every edge). Processing is serial — [`Self::process_next`]
    /// pops in push order.
    pub fn push_edge(&mut self, edge: UnitStateChanged) {
        self.pending.push_back(edge);
    }

    /// Process the next queued edge against the LIVE state (spec §7):
    /// stale discard, own-job suppression, and the out-of-band reaction
    /// (stop the other active members of the edge's group,
    /// `JobRemoved`-confirmed with the §4.2 conflicting-job rule; the
    /// edge's member keeps running).
    ///
    /// `Ok(None)` when the queue is empty. An error is a failed live read
    /// (bus trouble): the edge was dropped from the queue and the daemon
    /// re-syncs — a stale edge re-queued would race the re-sync anyway.
    /// The re-sync's own re-assertion (a dropped edge may have left the
    /// group double-active and no further edge may arrive) is the daemon
    /// resync path's job (ticket #18): `resync` here only refreshes the
    /// state snapshot.
    pub async fn process_next<C: SystemdCtl>(
        &mut self,
        ctl: &C,
        groups: &Groups,
        book: &mut JobBook,
    ) -> Result<Option<Reaction>, Error> {
        let Some(edge) = self.pending.pop_front() else {
            return Ok(None);
        };

        // Detection filter (spec §7): entering `activating`/`active` is
        // the start edge; `inactive`/`failed` require no reaction (zero
        // active is legal).
        if !matches!(
            edge.active_state,
            ActiveState::Active | ActiveState::Activating
        ) {
            return Ok(Some(Reaction::NoStartEdge));
        }

        // Non-member edge: ignored (the pump may forward every edge).
        let Some(group) = groups.group_of(&edge.unit) else {
            return Ok(Some(Reaction::NotAMember));
        };

        // Own-job suppression (spec §7 wording: an in-flight ussd START
        // job behind the edge — the switch ticket's bookkeeping): ussd's
        // own doing. The broader any-purpose rule is behaviorally
        // equivalent under systemd's per-unit job serialization (a
        // unit cannot transition to active while its own ussd stop job
        // runs), but the spec's rule is the one this implements.
        if book.own_active_units().contains(&edge.unit) {
            return Ok(Some(Reaction::Suppressed));
        }

        // The live state at execution time: the stale discard and the
        // stop-target set read the same snapshot.
        let members = groups.members(group).unwrap().to_vec();
        let states = ctl.list_states(&members).await.map_err(map_query_error)?;
        let live: HashMap<&str, ActiveState> = states
            .iter()
            .map(|(name, s)| (name.as_str(), s.active_state.clone()))
            .collect();

        if live.get(edge.unit.as_str()).cloned() != Some(ActiveState::Active) {
            // Stale: not `active` when the edge ran (e.g. just stopped by
            // a prior reaction) — dropped (spec §7).
            return Ok(Some(Reaction::Stale));
        }

        let others: Vec<&String> = members
            .iter()
            .filter(|m| {
                m.as_str() != edge.unit
                    && live.get(m.as_str()).cloned() == Some(ActiveState::Active)
            })
            .collect();
        if others.is_empty() {
            return Ok(Some(Reaction::NoConflicts));
        }

        let mut stopped = Vec::new();
        let mut failed = Vec::new();
        for member in others {
            // §4.2 conflicting-job rule. A failure is logged (journal via
            // stderr) and the edge consumed — the group stays
            // double-active until the next start edge re-asserts.
            match stop_unit_with_rule(ctl, book, member).await {
                Ok(()) => stopped.push(member.to_owned()),
                Err(e) => {
                    eprintln!("ussd watchdog: failed to stop {member} (group {group}): {e}");
                    failed.push(member.to_owned());
                }
            }
        }
        Ok(Some(Reaction::Stops { stopped, failed }))
    }
}

/// All group members in one flat list (re-sync / re-validation scope).
fn all_member_units(groups: &Groups) -> Vec<String> {
    groups
        .group_names()
        .into_iter()
        .flat_map(|g| groups.members(g).unwrap().iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobPurpose;
    use crate::systemdctl::{FakeSystemdCtl, JobHandle, JobResult, LoadState};

    fn members_of(groups: &Groups) -> Vec<String> {
        all_member_units(groups)
    }

    /// Drive `process_next` to Ready, settling the fake's pending stop
    /// jobs as they are issued.
    async fn run_process(
        fake: &FakeSystemdCtl,
        watchdog: &mut Watchdog,
        groups: &Groups,
        book: &mut JobBook,
        settle: fn(&FakeSystemdCtl, u64),
    ) -> Result<Option<Reaction>, Error> {
        let mut fut = Box::pin(watchdog.process_next(fake, groups, book));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut polls = 0u32;
        loop {
            polls += 1;
            assert!(
                polls <= 50,
                "poll cap exceeded — the handler looped instead of terminating"
            );
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(result) => return result,
                std::task::Poll::Pending => {
                    if let Some(id) = fake.jobs().iter().find_map(|j| {
                        (j.origin == crate::systemdctl::JobOrigin::Stop
                            && fake.pending_job(&j.unit) == Some(j.handle.id))
                        .then_some(j.handle.id)
                    }) {
                        settle(fake, id);
                    }
                }
            }
        }
    }

    /// A `JobRemoved` settle that also applies its live-state effect: a
    /// successful stop makes the unit read `inactive` (the fake never
    /// mutates state on its own).
    fn settle_done_inactive(f: &FakeSystemdCtl, id: u64) {
        let unit = f
            .jobs()
            .iter()
            .find(|j| j.handle.id == id)
            .unwrap()
            .unit
            .clone();
        f.settle_job(id, JobResult::Done);
        f.set_inactive(&unit);
    }

    // -- detection (bullet 1) -------------------------------------------------

    #[tokio::test]
    async fn start_edges_trigger_reaction_non_start_edges_do_not() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_active("b.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        // Entering `active`: the start edge — the other active member is
        // stopped, the edge's member keeps running.
        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            Reaction::Stops {
                stopped: vec!["a.service".into()],
                failed: vec![]
            }
        );
        assert_eq!(
            fake.get_unit_state("b.service").await.unwrap().active_state,
            ActiveState::Active,
            "the edge's member keeps running"
        );
        assert_eq!(
            fake.get_unit_state("a.service").await.unwrap().active_state,
            ActiveState::Inactive,
        );

        // Entering `inactive` / `failed`: no reaction (zero active is
        // legal), no jobs.
        for edge_state in [
            ActiveState::Inactive,
            ActiveState::Failed,
            ActiveState::Deactivating,
        ] {
            wd.push_edge(UnitStateChanged {
                unit: "a.service".into(),
                active_state: edge_state.clone(),
            });
            let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outcome, Reaction::NoStartEdge, "{edge_state:?}");
        }
        assert!(wd.is_empty());
    }

    // -- own-job suppression (bullet 2) ---------------------------------------

    #[tokio::test]
    async fn in_flight_ussd_job_suppresses_the_edge() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_active("b.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        // An in-flight ussd job behind the edge (either purpose): ussd's
        // own doing.
        book.record(JobHandle { id: 1 }, "b.service".into(), JobPurpose::Start);
        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, Reaction::Suppressed);
        // No stop was issued.
        assert!(fake.jobs().is_empty());

        // Spec §7 wording — suppression matches in-flight START jobs only:
        // an edge on a unit with an in-flight ussd STOP job is NOT
        // suppressed (unreachable in practice: per-unit job
        // serialization, but the rule is pinned here).
        book.settle(1);
        book.record(JobHandle { id: 2 }, "b.service".into(), JobPurpose::Stop);
        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            Reaction::Stops {
                stopped: vec!["a.service".into()],
                failed: vec![]
            }
        );
    }

    // -- out-of-band reaction (bullet 3) ---------------------------------------

    #[tokio::test]
    async fn reaction_stops_other_active_members_with_the_conflict_rule() {
        let fake = FakeSystemdCtl::new();
        // `a` carries a user-queued job: the watchdog's stop is rejected
        // and the §4.2 conflicting-job rule applies to it too.
        fake.set_active("a.service");
        fake.set_active("b.service");
        let queued = fake.queue_job("a.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        // Settle the queued job but keep `a` active (an in-flight start
        // completed) → the rule re-issues once; settle that too.
        let outcome = {
            let mut fut = Box::pin(wd.process_next(&fake, &groups, &mut book));
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut polls = 0u32;
            loop {
                polls += 1;
                assert!(polls <= 50, "poll cap exceeded — the handler looped");
                match fut.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => break result,
                    std::task::Poll::Pending => {
                        if fake.pending_job("a.service") == Some(queued.id) {
                            fake.settle_job(queued.id, JobResult::Done);
                        }
                        if let Some(id) = fake.pending_job("a.service") {
                            fake.settle_job(id, JobResult::Done);
                            fake.set_inactive("a.service");
                        }
                    }
                }
            }
        }
        .unwrap();
        assert_eq!(
            outcome,
            Some(Reaction::Stops {
                stopped: vec!["a.service".into()],
                failed: vec![]
            })
        );
        // Exactly one (re-issued) stop job — the first was rejected.
        assert_eq!(
            fake.jobs()
                .iter()
                .filter(|j| j.origin == crate::systemdctl::JobOrigin::Stop && j.unit == "a.service")
                .count(),
            1
        );
        // X keeps running.
        assert_eq!(
            fake.get_unit_state("b.service").await.unwrap().active_state,
            ActiveState::Active
        );
    }

    // -- stale discard (bullet 4) ------------------------------------------------

    #[tokio::test]
    async fn stale_edge_is_discarded() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_inactive("b.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        // The edge says `b` became active, but by execution time `b` is
        // no longer active (e.g. just stopped out-of-band) — dropped.
        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome, Reaction::Stale);
        assert!(fake.jobs().is_empty());
    }

    // -- convergence (bullet 5) ---------------------------------------------------

    #[tokio::test]
    async fn back_to_back_out_of_band_starts_converge_to_one_active() {
        let fake = FakeSystemdCtl::new();
        for unit in ["a.service", "b.service", "c.service"] {
            fake.set_active(unit);
        }
        let mut groups = Groups::default();
        for unit in ["a.service", "b.service", "c.service"] {
            groups.add_member("vpn", unit);
        }
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        // Back-to-back out-of-band starts on G.
        for unit in ["a.service", "b.service", "c.service"] {
            wd.push_edge(UnitStateChanged {
                unit: unit.into(),
                active_state: ActiveState::Active,
            });
        }

        let mut reactions = Vec::new();
        while !wd.is_empty() {
            let r = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
                .await
                .unwrap();
            reactions.push(r);
        }

        // The first edge is non-stale and stops the other two; the other
        // two edges then see their members no longer active and are
        // stale — no zero-active, no lingering double-active.
        let mut active = Vec::new();
        for u in members_of(&groups) {
            if fake.get_unit_state(&u).await.unwrap().active_state == ActiveState::Active {
                active.push(u);
            }
        }
        assert_eq!(
            active,
            vec!["a.service".to_string()],
            "exactly one active member remains (the first non-stale edge)"
        );
        // No member was ever started by ussd — only stops.
        assert!(
            fake.jobs()
                .iter()
                .all(|j| j.origin == crate::systemdctl::JobOrigin::Stop)
        );
        assert!(reactions.contains(&Some(Reaction::Stale)));
        assert!(
            reactions
                .iter()
                .any(|r| matches!(r, Some(Reaction::Stops { .. })))
        );
    }

    #[tokio::test]
    async fn incremental_starts_converge_to_the_last_starter() {
        // The spec's in-practice interleaving (unlike the all-active-from
        // the-start test above): edges arrive one at a time, each as its
        // start completes — an earlier edge sees the later starters not
        // yet active, so the LAST non-stale edge's member survives.
        let fake = FakeSystemdCtl::new();
        for unit in ["a.service", "b.service", "c.service"] {
            fake.set_inactive(unit);
        }
        let mut groups = Groups::default();
        for unit in ["a.service", "b.service", "c.service"] {
            groups.add_member("vpn", unit);
        }
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        let mut last: Option<Reaction> = None;
        for unit in ["a.service", "b.service", "c.service"] {
            // Each out-of-band start completes: the member becomes
            // live-active and its edge is queued, then processed against
            // the live state at that moment.
            fake.set_active(unit);
            wd.push_edge(UnitStateChanged {
                unit: unit.into(),
                active_state: ActiveState::Active,
            });
            last = run_process(&fake, &mut wd, &groups, &mut book, settle_done_inactive)
                .await
                .unwrap();
        }

        // a's edge: no conflicts (first active) · b's edge: stops a ·
        // c's edge: stops b.
        assert_eq!(
            last,
            Some(Reaction::Stops {
                stopped: vec!["b.service".into()],
                failed: vec![]
            })
        );
        // The last starter survives: no zero-active, no double-active.
        let mut active = Vec::new();
        for u in members_of(&groups) {
            if fake.get_unit_state(&u).await.unwrap().active_state == ActiveState::Active {
                active.push(u);
            }
        }
        assert_eq!(active, vec!["c.service".to_string()]);
    }

    // -- failed stop (bullet 6) ---------------------------------------------------

    #[tokio::test]
    async fn failed_stop_logged_group_stays_double_active_until_reassert() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service"); // the ignores-stop member
        fake.set_active("b.service"); // the out-of-band starter
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");
        let mut wd = Watchdog::new();
        let mut book = JobBook::default();

        // Round 1: `b`'s edge → stop `a`; `a`'s stop job fails (it ignores
        // stop). The edge is consumed with the failure logged; the group
        // stays double-active.
        wd.push_edge(UnitStateChanged {
            unit: "b.service".into(),
            active_state: ActiveState::Active,
        });
        let settle_failed = |f: &FakeSystemdCtl, id: u64| {
            f.settle_job(id, JobResult::Failed);
        };
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_failed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            Reaction::Stops {
                stopped: vec![],
                failed: vec!["a.service".into()]
            }
        );
        assert_eq!(
            fake.get_unit_state("a.service").await.unwrap().active_state,
            ActiveState::Active,
            "the group stays double-active"
        );
        assert_eq!(
            fake.get_unit_state("b.service").await.unwrap().active_state,
            ActiveState::Active
        );

        // Re-assert: a NEW out-of-band start (`c`) triggers another
        // reaction round — the stopped one goes down, the ignores-stop
        // one fails again and keeps running.
        fake.set_active("c.service");
        groups.add_member("vpn", "c.service");
        wd.push_edge(UnitStateChanged {
            unit: "c.service".into(),
            active_state: ActiveState::Active,
        });
        let settle_round2 = |f: &FakeSystemdCtl, id: u64| {
            let unit = f
                .jobs()
                .iter()
                .find(|j| j.handle.id == id)
                .unwrap()
                .unit
                .clone();
            if unit == "a.service" {
                f.settle_job(id, JobResult::Failed); // still ignoring stop
            } else {
                f.settle_job(id, JobResult::Done);
                f.set_inactive(&unit);
            }
        };
        let outcome = run_process(&fake, &mut wd, &groups, &mut book, settle_round2)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome,
            Reaction::Stops {
                stopped: vec!["b.service".into()],
                failed: vec!["a.service".into()]
            }
        );
        let mut active = Vec::new();
        for u in members_of(&groups) {
            if fake.get_unit_state(&u).await.unwrap().active_state == ActiveState::Active {
                active.push(u);
            }
        }
        assert_eq!(
            active,
            vec!["a.service".to_string(), "c.service".to_string()]
        );
    }

    // -- re-sync (bullet 7) ---------------------------------------------------------

    #[tokio::test]
    async fn resync_on_startup_and_after_reconnect() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_inactive("b.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");

        // Startup: subscribe + fresh snapshot of all members' live state.
        assert!(!fake.is_subscribed());
        let snapshot = Watchdog::resync(&fake, &groups).await.unwrap();
        assert!(fake.is_subscribed());
        assert_eq!(
            snapshot
                .iter()
                .map(|(u, s)| (u.as_str(), s.active_state.clone()))
                .collect::<HashMap<_, _>>(),
            HashMap::from([
                ("a.service", ActiveState::Active),
                ("b.service", ActiveState::Inactive),
            ])
        );

        // Reconnect: the per-connection subscription is gone; resync
        // re-subscribes and re-reads the (changed) live state.
        fake.reset_connection();
        assert!(!fake.is_subscribed());
        fake.set_active("b.service"); // the world moved while disconnected
        let snapshot = Watchdog::resync(&fake, &groups).await.unwrap();
        assert!(fake.is_subscribed());
        let b = snapshot.iter().find(|(u, _)| u == "b.service").unwrap();
        assert_eq!(b.1.active_state, ActiveState::Active);
    }

    // -- unit files (bullet 8) --------------------------------------------------------

    #[tokio::test]
    async fn files_changed_revalidates_without_membership_changes() {
        let fake = FakeSystemdCtl::new();
        fake.set_active("a.service");
        fake.set_active("b.service");
        let mut groups = Groups::default();
        groups.add_member("vpn", "a.service");
        groups.add_member("vpn", "b.service");

        // The event is delivered on the seam channel (the daemon's pump
        // calls revalidate on it).
        let mut rx = fake.unit_files_changed();
        fake.emit_files_changed();
        assert!(rx.recv().await.is_ok());

        // A member unit became masked on disk: the re-validation reads it,
        // and nothing changes the membership.
        fake.set_masked("a.service");
        let fresh = Watchdog::revalidate_unit_files(&fake, &groups)
            .await
            .unwrap();
        let a = fresh.iter().find(|(u, _)| u == "a.service").unwrap();
        assert_eq!(a.1.load_state, LoadState::Masked);
        assert_eq!(
            groups.members("vpn").unwrap(),
            &["a.service", "b.service"][..],
            "no automatic membership changes"
        );
    }
}
