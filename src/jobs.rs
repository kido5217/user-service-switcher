//! ussd's own-operation job bookkeeping (spec §4.2/§7).
//!
//! Every job ussd issues through the [`SystemdCtl`] seam is recorded here,
//! keyed by the systemd job id — the `JobRemoved` correlation key (the
//! real backend's job object path
//! `/org/freedesktop/systemd1/job/<id>` carries it as its last component;
//! the seam surfaces the id).
//!
//! The §7 watchdog (the next slice) consumes this: a `UnitStateChanged`
//! edge matching an in-flight ussd job is ussd's own doing — suppressed,
//! not an external event. Observing the job's `JobRemoved` settles the
//! record ([`JobBook::settle`]).

use std::collections::{HashMap, HashSet};

use crate::systemdctl::JobHandle;

/// Why ussd queued the job (which phase of which command issued it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPurpose {
    /// `StartUnit(target, "replace")` — a target's activation.
    Start,
    /// `StopUnit(unit, "fail")` — a member's deactivation (the stop phase
    /// of `start`, a standalone `stop`, or `remove`'s active member).
    Stop,
}

/// A recorded job: unit + purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookedJob {
    pub unit: String,
    pub purpose: JobPurpose,
}

/// Every job ussd has issued, keyed by job id, until its `JobRemoved` is
/// observed.
#[derive(Debug, Default)]
pub struct JobBook {
    jobs: HashMap<u64, BookedJob>,
}

impl JobBook {
    /// Record an issued job — at issue time, before the await, so a
    /// `UnitStateChanged` edge arriving while the job is in flight
    /// already matches it.
    pub fn record(&mut self, job: JobHandle, unit: String, purpose: JobPurpose) {
        self.jobs.insert(job.id, BookedJob { unit, purpose });
    }

    /// Settle a recorded job (its `JobRemoved` was observed) and return
    /// the record. Unknown ids return `None` — the job was not ussd's
    /// (e.g. a user-queued job the §4.2 conflicting-job rule waited on).
    pub fn settle(&mut self, id: u64) -> Option<BookedJob> {
        self.jobs.remove(&id)
    }

    /// The units with an in-flight `Start` job — whose `ActiveState`
    /// edges are ussd's own doing (§7 suppression).
    pub fn own_active_units(&self) -> HashSet<String> {
        self.jobs
            .values()
            .filter(|j| j.purpose == JobPurpose::Start)
            .map(|j| j.unit.clone())
            .collect()
    }

    /// Whether `unit` has any in-flight ussd job (either purpose).
    pub fn has_in_flight(&self, unit: &str) -> bool {
        self.jobs.values().any(|j| j.unit == unit)
    }

    /// In-flight count (test introspection).
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(id: u64) -> JobHandle {
        JobHandle { id }
    }

    #[test]
    fn record_settle_roundtrip() {
        let mut book = JobBook::default();
        book.record(handle(1), "a.service".into(), JobPurpose::Start);
        book.record(handle(2), "b.service".into(), JobPurpose::Stop);
        assert_eq!(book.len(), 2);

        let settled = book.settle(1);
        assert_eq!(
            settled,
            Some(BookedJob {
                unit: "a.service".into(),
                purpose: JobPurpose::Start
            })
        );
        assert!(book.has_in_flight("b.service"));
        assert!(!book.has_in_flight("a.service"));
        // A job settles exactly once.
        assert_eq!(book.settle(1), None);
    }

    #[test]
    fn settle_of_unrecorded_job_is_none() {
        let mut book = JobBook::default();
        // A user-queued job the §4.2 rule waited on: never ussd's.
        assert_eq!(book.settle(99), None);
        assert!(book.is_empty());
    }

    #[test]
    fn own_active_units_lists_only_start_purposes() {
        let mut book = JobBook::default();
        book.record(handle(1), "a.service".into(), JobPurpose::Start);
        book.record(handle(2), "b.service".into(), JobPurpose::Stop);
        book.record(handle(3), "c.service".into(), JobPurpose::Start);

        assert_eq!(
            book.own_active_units(),
            HashSet::from(["a.service".to_string(), "c.service".to_string()])
        );
        // Settling the start job removes the unit from the set.
        book.settle(1);
        assert_eq!(
            book.own_active_units(),
            HashSet::from(["c.service".to_string()])
        );
    }

    #[test]
    fn has_in_flight_covers_both_purposes() {
        let mut book = JobBook::default();
        book.record(handle(1), "a.service".into(), JobPurpose::Stop);
        assert!(book.has_in_flight("a.service"));
        assert!(!book.has_in_flight("b.service"));
    }
}
