//! Integration tests (spec §11, ticket #24): `uss` + `ussd` against the
//! LIVE user manager on the maintainer's host.
//!
//! Run with `just integration` (dev shell): it checks for a live session
//! and runs `cargo test -- --ignored --test-threads=1` — the single-thread
//! flag is REQUIRED for this binary: every test operates on the ONE live
//! user manager (shared state file, shared `ussd.service` install), so
//! parallel tests would race each other. Plain `cargo test` (the merge
//! gate) never runs these: they are `#[ignore]`d.
//!
//! Coverage (the ticket's five scenarios): the full switch cycle, status
//! byte-exactness, the out-of-band start -> watchdog reaction (the ticket
//! names `systemd-run --user`; a transient `run-*.service` cannot be a
//! group member, so the faithful reading is the manager's own start verb
//! on a member), bootstrap from a clean `~/.config/uss` (the installed
//! `ussd.service` is guarded, not removed — the bootstrap test exercises
//! the rewrite path, not a fresh install), and
//! state-file survival across a daemon stop (the ticket's "stop the
//! manager" variant is the e2e-watchdog vertical's kill -9 scenario).
//!
//! Fixture discipline: the unit files live in the repo
//! (`tests/fixtures/fixture.service.in`) and are installed under the
//! session's REAL user unit dir (`$XDG_CONFIG_HOME/systemd/user`) with
//! the distinctive `uss-int-` prefix — a single live manager only sees
//! ITS OWN search path, so a fully isolated unit dir is invisible to it.
//! Every test sweeps leftovers first, stops the daemon before touching
//! the state file (the running daemon would otherwise rewrite it from
//! memory), and restores the host's steady state on teardown: `ussd`
//! installed + enabled and STOPPED (the next `uss` starts it via the
//! bootstrap), the user's pre-run state restored if the user had any
//! (else no groups dir), no fixture units. Crash-safe: the guards' disk
//! baselines (`~/.config/uss.bak`, `~/.config/systemd/user/
//! ussd.service.bak[-link]`) survive a SIGKILL mid-run and repair on the
//! next run's start (see the guard docs).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const ALL_FIXTURES: [&str; 4] = ["uss-int-a", "uss-int-b", "uss-int-w1", "uss-int-w2"];

// -- session environment -----------------------------------------------------

fn user_config_dir() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(".config"))
            .expect("HOME unset — a live user session is required (just integration)"),
    }
}

fn unit_dir() -> PathBuf {
    user_config_dir().join("systemd").join("user")
}

fn cfg_uss_dir() -> PathBuf {
    user_config_dir().join("uss")
}

fn sleep_bin() -> String {
    for candidate in [
        "/run/current-system/sw/bin/sleep", // NixOS hosts
        "/bin/sleep",
        "/usr/bin/sleep",
    ] {
        if Path::new(candidate).exists() {
            return candidate.to_owned();
        }
    }
    panic!("no sleep binary found — fixture units need one of the known paths");
}

/// `systemctl --user …`, asserting success and returning stdout.
fn systemctl(args: &[&str]) -> String {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .expect("spawn systemctl");
    assert!(
        out.status.success(),
        "systemctl --user {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("systemctl stdout utf-8")
        .trim()
        .to_owned()
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn wait_unit_active(unit: &str, want: bool, deadline: Duration) {
    let end = Instant::now() + deadline;
    loop {
        if unit_active(unit) == want {
            return;
        }
        assert!(
            Instant::now() < end,
            "{unit} did not reach active={want} within {deadline:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run the real `uss` binary (fresh dev-profile build via
/// `CARGO_BIN_EXE_uss`) with the inherited session environment.
fn run_uss(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_uss"))
        .args(args)
        .output()
        .expect("spawn uss");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_uss_ok(args: &[&str]) {
    let (code, out, err) = run_uss(args);
    assert_eq!(
        (code, &out, &err),
        (0, &String::new(), &String::new()),
        "uss {} failed: exit {code}, stderr: {err}",
        args.join(" ")
    );
}

/// Stop the daemon (and clear its failure accounting: the suite's rapid
/// stop/start cycles exhaust the default `StartLimitBurst` of 5 starts per
/// 10 s — systemd's own remedy for a `start-limit-hit` unit is to reset
/// its accounting before the next start; a no-op when the unit isn't
/// failed. Ticket #24 reviewer, empirically validated on the host: 6
/// rapid start cycles fail the 6th exactly like this does without it).
fn stop_ussd() {
    systemctl(&["stop", "ussd.service"]);
    wait_unit_active("ussd.service", false, Duration::from_secs(10));
    let _ = systemctl(&["reset-failed", "ussd.service"]);
}

// -- fixture units -----------------------------------------------------------

fn render_fixture(unit: &str) -> String {
    let template = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/fixture.service.in"
    ))
    .expect("read the fixture template from the repo");
    template
        .replace("@UNIT@", unit)
        .replace("@SLEEP@", &sleep_bin())
}

fn install_fixtures(units: &[&str]) {
    let dir = unit_dir();
    fs::create_dir_all(&dir).expect("user unit dir");
    for unit in units {
        fs::write(dir.join(format!("{unit}.service")), render_fixture(unit))
            .expect("write the fixture unit");
    }
    systemctl(&["daemon-reload"]);
    for unit in units {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if systemctl(&[
                "show",
                "-p",
                "LoadState",
                "--value",
                &format!("{unit}.service"),
            ]) == "loaded"
            {
                break;
            }
            assert!(
                Instant::now() < end,
                "{unit}.service not loaded after daemon-reload"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn remove_fixtures(units: &[&str]) {
    for unit in units {
        let file = unit_dir().join(format!("{unit}.service"));
        if file.exists() {
            let _ = systemctl(&["stop", &format!("{unit}.service")]);
            fs::remove_file(&file).expect("remove the fixture unit");
        }
    }
    if units.is_empty() {
        return;
    }
    systemctl(&["daemon-reload"]);
}

// -- state guards -------------------------------------------------------------
/// The baseline paths the guards write on every `new()` (the crash-safety
/// mechanism below).
const STATE_BACKUP: &str = "uss.bak";
const STATE_MARKER: &str = ".guard";
const UNIT_BACKUP: &str = "ussd.service.bak";
const UNIT_BACKUP_LINK: &str = "ussd.service.bak-link";

/// Moves the live `~/.config/uss` dir aside and restores it on drop. Tests
/// run single-threaded and each starts from a clean slate.
///
/// Crash-safe against a SIGKILL mid-run (the in-memory version dies with
/// the process): the baseline lives at the FIXED path `~/.config/uss.bak`
/// and is always written on `new()` — an empty dir with a marker when the
/// user has no state. A killed run's baseline survives on disk, and the
/// next `new()` repairs from it: the live dir is the killed run's state
/// (the suite always ends with the live dir absent or restored), and the
/// baseline holds the user's pre-run state.
struct StateGuard;

impl StateGuard {
    fn new() -> Self {
        let cfg = user_config_dir();
        let live = cfg.join("uss");
        let backup = cfg.join(STATE_BACKUP);
        // Sweep stale pid-suffixed backups from the pre-crash-safety
        // design (ticket #24 review).
        if let Ok(entries) = fs::read_dir(&cfg) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.starts_with("uss.bak-"))
                {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        if backup.exists() {
            // A previous run was killed mid-suite: the baseline holds the
            // user's pre-run state (or the marker alone = the user had
            // none); the live dir, if present, is the killed run's state.
            let _ = fs::remove_dir_all(&live);
        } else {
            fs::create_dir_all(&backup).expect("create the state baseline");
            fs::write(backup.join(STATE_MARKER), "").expect("state baseline marker");
            if live.exists() {
                // The user's pre-run state goes into the baseline; the
                // marker's absence is what marks "the user had state".
                fs::rename(&live, &backup).expect("move the uss state dir aside");
                let _ = fs::remove_file(backup.join(STATE_MARKER));
            }
        }
        StateGuard
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let cfg = user_config_dir();
        let live = cfg.join("uss");
        let backup = cfg.join(STATE_BACKUP);
        let _ = fs::remove_dir_all(&live); // the test's own state
        if backup.exists() && !backup.join(STATE_MARKER).exists() {
            // The baseline holds the user's pre-run state — restore it.
            let _ = fs::rename(&backup, &live);
        } else {
            let _ = fs::remove_dir_all(&backup);
        }
    }
}

/// Saves the installed `ussd.service` unit file and its enable link;
/// restores both (plus a daemon-reload) on drop. The bootstrap test
/// rewrites the unit file (its `ExecStart` points at the binary under
/// test); the other tests leave it alone but take the guard cheaply.
///
/// Crash-safe against a SIGKILL mid-run: the baseline lives at the fixed
/// paths `ussd.service.bak` (the unit file's bytes) and
/// `ussd.service.bak-link` (the enable link's target), written on `new()`
/// when the live file exists. A killed run's baseline survives on disk;
/// the next `new()` restores the unit file from it (the live file is the
/// killed run's rewrite — a dead worktree path, say) and reconciles the
/// enable link against the baseline.
struct UssdUnitGuard {
    unit_file: PathBuf,
    wants_link: PathBuf,
}

impl UssdUnitGuard {
    fn new() -> Self {
        let unit_dir = unit_dir();
        let unit_file = unit_dir.join("ussd.service");
        let wants_link = unit_dir.join("default.target.wants").join("ussd.service");
        let unit_backup = unit_dir.join(UNIT_BACKUP);
        let link_backup = unit_dir.join(UNIT_BACKUP_LINK);
        if unit_backup.exists() {
            // A previous run was killed: the baseline is truth. Restore the
            // unit file over the killed run's rewrite and reconcile the
            // enable link against the baseline.
            if unit_file.exists() {
                fs::copy(&unit_backup, &unit_file).expect("restore the baseline unit file");
            }
            let link_was = link_backup
                .exists()
                .then(|| fs::read_to_string(&link_backup).expect("baseline link target"));
            let link_is = wants_link.symlink_metadata().is_ok();
            match (link_was.as_deref(), link_is) {
                (Some(target), false) => {
                    let _ = std::os::unix::fs::symlink(target, &wants_link);
                }
                (None, true) => {
                    let _ = fs::remove_file(&wants_link);
                }
                _ => {}
            }
        } else if unit_file.exists() {
            fs::copy(&unit_file, &unit_backup).expect("save the unit baseline");
            if let Ok(target) = fs::read_link(&wants_link) {
                let link_target = target.to_string_lossy();
                let _ = fs::write(&link_backup, link_target.as_ref());
            }
        }
        UssdUnitGuard {
            unit_file,
            wants_link,
        }
    }
}

impl Drop for UssdUnitGuard {
    fn drop(&mut self) {
        let unit_dir = unit_dir();
        let unit_backup = unit_dir.join(UNIT_BACKUP);
        let link_backup = unit_dir.join(UNIT_BACKUP_LINK);
        if unit_backup.exists() {
            fs::copy(&unit_backup, &self.unit_file).expect("restore the unit file");
        } else {
            let _ = fs::remove_file(&self.unit_file);
        }
        match fs::read_to_string(&link_backup) {
            Ok(target) => {
                if !self.wants_link.symlink_metadata().is_ok() {
                    let _ = std::os::unix::fs::symlink(&target, &self.wants_link);
                }
            }
            Err(_) => {
                let _ = fs::remove_file(&self.wants_link);
            }
        }
        let _ = fs::remove_file(&unit_backup);
        let _ = fs::remove_file(&link_backup);
        let _ = systemctl(&["daemon-reload"]);
    }
}

// -- the scenarios -------------------------------------------------------------

/// The deterministic start of every test: daemon stopped (it would
/// rewrite the state file from memory), fixture leftovers swept.
fn clean_slate(units: &[&str]) -> StateGuard {
    stop_ussd();
    remove_fixtures(units);
    remove_fixtures(&ALL_FIXTURES);
    StateGuard::new()
}

fn teardown(units: &[&str]) {
    stop_ussd();
    remove_fixtures(units);
}

#[test]
#[ignore = "live user session required (ticket #24); run via `just integration`"]
fn full_switch_cycle() {
    let units = ["uss-int-a", "uss-int-b"];
    let _state = clean_slate(&units);
    install_fixtures(&units);

    // First use on the stopped daemon: bootstrap installs + starts ussd.
    run_uss_ok(&["itg", "add", "uss-int-a"]);
    run_uss_ok(&["itg", "add", "uss-int-b"]);
    wait_unit_active("ussd.service", true, Duration::from_secs(15));

    // The switch (spec §4.2): starting b while a is active stops a
    // (JobRemoved-confirmed) only then starts b.
    run_uss_ok(&["itg", "start", "uss-int-a"]);
    wait_unit_active("uss-int-a.service", true, Duration::from_secs(10));
    run_uss_ok(&["itg", "start", "uss-int-b"]);
    wait_unit_active("uss-int-b.service", true, Duration::from_secs(10));
    wait_unit_active("uss-int-a.service", false, Duration::from_secs(10));

    // Status: byte-exact §4.3 (add order, the running member suffixed).
    let (code, out, err) = run_uss(&["itg"]);
    assert_eq!(code, 0, "status: {err}");
    assert_eq!(
        out,
        "itg\n  uss-int-a.service\n  uss-int-b.service - Active\n"
    );

    run_uss_ok(&["itg", "stop", "uss-int-b"]);
    wait_unit_active("uss-int-b.service", false, Duration::from_secs(10));
    teardown(&units);
}

#[test]
#[ignore = "live user session required (ticket #24); run via `just integration`"]
fn status_rendering_is_byte_exact() {
    let units = ["uss-int-a", "uss-int-b"];
    let _state = clean_slate(&units);
    install_fixtures(&units);

    run_uss_ok(&["itg", "add", "uss-int-a"]);
    run_uss_ok(&["itg", "add", "uss-int-b"]);

    // All inactive: plain rows, add order, trailing newline per row.
    let (code, out, err) = run_uss(&["itg"]);
    assert_eq!(code, 0, "status: {err}");
    assert_eq!(out, "itg\n  uss-int-a.service\n  uss-int-b.service\n");

    // The bare `uss` (all groups) renders the same single group.
    let (code, out, err) = run_uss(&[]);
    assert_eq!(code, 0, "bare status: {err}");
    assert_eq!(out, "itg\n  uss-int-a.service\n  uss-int-b.service\n");

    // One active: the marker suffix, the other row bare.
    run_uss_ok(&["itg", "start", "uss-int-a"]);
    wait_unit_active("uss-int-a.service", true, Duration::from_secs(10));
    let (code, out, err) = run_uss(&[]);
    assert_eq!(code, 0, "status: {err}");
    assert_eq!(
        out,
        "itg\n  uss-int-a.service - Active\n  uss-int-b.service\n"
    );

    run_uss_ok(&["itg", "stop", "uss-int-a"]);
    wait_unit_active("uss-int-a.service", false, Duration::from_secs(10));
    teardown(&units);
}

#[test]
#[ignore = "live user session required (ticket #24); run via `just integration`"]
fn out_of_band_start_reaction_stops_the_other_member() {
    let units = ["uss-int-w1", "uss-int-w2"];
    let _state = clean_slate(&units);
    install_fixtures(&units);

    run_uss_ok(&["itw", "add", "uss-int-w1"]);
    run_uss_ok(&["itw", "add", "uss-int-w2"]);
    run_uss_ok(&["itw", "start", "uss-int-w1"]);
    wait_unit_active("uss-int-w1.service", true, Duration::from_secs(10));

    // The out-of-band start (spec §7 edge): the manager alone knows —
    // the watchdog's reaction must stop the other member within ~1 s
    // (the ticket's bound; the detection itself is signal-driven,
    // ~5-6 ms per the ticket-#5 evidence).
    let started = Instant::now();
    systemctl(&["start", "uss-int-w2.service"]);
    let deadline = started + Duration::from_secs(1);
    loop {
        if !unit_active("uss-int-w1.service") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the watchdog did not stop w1 within 1 s of the out-of-band start"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        unit_active("uss-int-w2.service"),
        "the edge's member keeps running"
    );

    let (code, out, err) = run_uss(&["itw"]);
    assert_eq!(code, 0, "status: {err}");
    assert_eq!(
        out,
        "itw\n  uss-int-w1.service\n  uss-int-w2.service - Active\n"
    );

    run_uss_ok(&["itw", "stop", "uss-int-w2"]);
    wait_unit_active("uss-int-w2.service", false, Duration::from_secs(10));
    teardown(&units);
}

#[test]
#[ignore = "live user session required (ticket #24); run via `just integration`"]
fn bootstrap_from_clean_installs_and_starts_ussd() {
    let units = ["uss-int-a"];
    stop_ussd();
    remove_fixtures(&units);
    remove_fixtures(&ALL_FIXTURES);
    let _unit = UssdUnitGuard::new(); // save the installed unit file + link
    let _state = StateGuard::new(); // a clean ~/.config/uss

    // FIRST USE: bare `uss` — the full §8 bootstrap (write the unit file,
    // reload, enable, start, wait for JobRemoved) then the command.
    let (code, out, err) = run_uss(&[]);
    assert_eq!((code, out), (0, String::new()), "first use: {err}");
    wait_unit_active("ussd.service", true, Duration::from_secs(15));

    // The unit file the bootstrap wrote points at the binary under test
    // (absolute — a relative argv0 once produced a relative ExecStart).
    let written =
        fs::read_to_string(unit_dir().join("ussd.service")).expect("bootstrap wrote ussd.service");
    assert!(
        written.contains(&format!("ExecStart={}\n", env!("CARGO_BIN_EXE_ussd"))),
        "ExecStart embeds the absolute ussd path:\n{written}"
    );
    // Enabled (persistent symlink).
    assert!(
        unit_dir()
            .join("default.target.wants")
            .join("ussd.service")
            .symlink_metadata()
            .is_ok(),
        "bootstrap enabled ussd.service"
    );

    // The daemon is live: a mutating command works end to end.
    install_fixtures(&units);
    run_uss_ok(&["itg", "add", "uss-int-a"]);

    teardown(&units);
}

#[test]
#[ignore = "live user session required (ticket #24); run via `just integration`"]
fn state_file_survives_daemon_stop() {
    let units = ["uss-int-a"];
    let _state = clean_slate(&units);
    install_fixtures(&units);

    // `uss` bootstraps the daemon and persists the group.
    run_uss_ok(&["its", "add", "uss-int-a"]);
    wait_unit_active("ussd.service", true, Duration::from_secs(15));
    let state_file = cfg_uss_dir().join("groups.json");
    let contents = fs::read_to_string(&state_file).expect("groups.json persisted");
    assert!(
        contents.contains("\"its\""),
        "the group is in the state file"
    );

    // Stop the daemon: the state file is untouched. `uss` again -> the
    // bootstrap restarts the daemon, which re-syncs from systemd and
    // reads the same state file — the group must still be there.
    stop_ussd();
    let (code, out, err) = run_uss(&["its"]);
    assert_eq!(code, 0, "state survives the daemon restart: {err}");
    assert_eq!(out, "its\n  uss-int-a.service\n");

    teardown(&units);
}
