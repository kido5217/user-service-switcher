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
//! on a member), bootstrap from a clean `~/.config/uss` + unit dir, and
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
//! installed + enabled + running, no `groups.json`, no fixture units.
//! The ussd.service unit file + its enable link are saved and restored by
//! `UssdUnitGuard` (the bootstrap test rewrites both).

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

fn stop_ussd() {
    systemctl(&["stop", "ussd.service"]);
    wait_unit_active("ussd.service", false, Duration::from_secs(10));
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

/// Moves the live `~/.config/uss` dir aside (under the process id, so a
/// crashed run's backup is distinguishable and swept) and restores it on
/// drop. Tests run single-threaded and each starts from a clean slate.
struct StateGuard {
    backup: PathBuf,
}

impl StateGuard {
    fn new() -> Self {
        let live = cfg_uss_dir();
        let backup = live.with_file_name(format!("uss.bak-{}", std::process::id()));
        let _ = fs::remove_dir_all(&backup); // a crashed run's leftover
        if live.exists() {
            fs::rename(&live, &backup).expect("move the uss state dir aside");
        }
        StateGuard { backup }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let live = cfg_uss_dir();
        if live.exists() {
            let _ = fs::remove_dir_all(&live);
        }
        if self.backup.exists() {
            let _ = fs::rename(&self.backup, &live);
        }
    }
}

/// Saves the installed `ussd.service` unit file and its enable link;
/// restores both (plus a daemon-reload) on drop. The bootstrap test
/// rewrites the unit file (its `ExecStart` points at the binary under
/// test); the other tests leave it alone but take the guard cheaply.
struct UssdUnitGuard {
    unit_file: PathBuf,
    wants_link: PathBuf,
    unit_file_was: Option<Vec<u8>>,
    wants_link_was: bool,
}

impl UssdUnitGuard {
    fn new() -> Self {
        let unit_file = unit_dir().join("ussd.service");
        let wants_link = unit_dir().join("default.target.wants").join("ussd.service");
        UssdUnitGuard {
            unit_file_was: unit_file
                .exists()
                .then(|| fs::read(&unit_file).expect("read ussd.service")),
            wants_link_was: wants_link.symlink_metadata().is_ok(),
            unit_file,
            wants_link,
        }
    }
}

impl Drop for UssdUnitGuard {
    fn drop(&mut self) {
        match &self.unit_file_was {
            Some(bytes) => {
                let _ = fs::write(&self.unit_file, bytes);
            }
            None => {
                let _ = fs::remove_file(&self.unit_file);
            }
        }
        if self.wants_link_was {
            // The daemon wrote a new symlink during bootstrap: relink.
            if !self.wants_link.symlink_metadata().is_ok() {
                let _ = std::os::unix::fs::symlink(
                    self.unit_file
                        .strip_prefix(unit_dir())
                        .expect("relative unit path"),
                    &self.wants_link,
                );
            }
        } else {
            let _ = fs::remove_file(&self.wants_link);
        }
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
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "reaction took {elapsed:?}"
    );
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
