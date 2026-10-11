//! CLI tests (spec §11): the real `uss` binary against a fake ussd.
//!
//! The harness binds a `UnixListener` at `$XDG_RUNTIME_DIR/uss/ussd.sock`
//! under a temp dir (spec §6 transport), scripts one §6 response per
//! connection, and records the exact request lines the client sent. The
//! child runs with `USS_TEST_SKIP_BOOTSTRAP` set (the §8 debug-only test
//! hook) so steps 1–4 — the whole D-Bus bootstrap — are short-circuited:
//! offline, hermetic, and hostile by construction (every spawn also
//! carries a BOGUS `DBUS_SESSION_BUS_ADDRESS`: a bootstrap that leaked
//! past the hook would exit 7 and fail the byte-exact assertions).
//!
//! Coverage: every §4.4 row's exit code + exact stderr text reachable
//! without D-Bus, and every §4.3 format case. The two row-7 templates
//! that live INSIDE the bootstrap (`ussd binary not found`, `ussd failed
//! to start`) are unreachable for the real binary without a live user
//! manager — step 1 gates them — and stay covered by the `FakeSystemdCtl`
//! bootstrap tests in `uss_client` (they run in the same `cargo test`
//! gate). The row-7 timeout test waits the full spec 30 s on purpose:
//! the point of it is that the binary's own `RESPONSE_TIMEOUT` is 30 s
//! and the printed reason cell is byte-exact.
//!
//! Profile: the §8 hook is compiled into DEBUG builds only — so the
//! socket-path cases self-skip under a release profile (the nix build's
//! checkPhase), while every profile-independent case (usage, help,
//! environment-before-socket) runs everywhere. The `cargo test` gate is
//! the dev profile, so no case is ever skipped where the gate runs.
//!
//! Response literals below are the shapes the REAL daemon encodes
//! (`Response::ok/ok_empty/err` — kebab-case codes, `message` verbatim
//! per §6; `daemon::dispatch`/`err_response` are the source), so the
//! client is locked against the daemon it actually talks to.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::{fs, thread};

/// The bogus bus address every spawned `uss` gets (see the module docs).
const BOGUS_BUS: &str = "unix:path=/uss-tests-no-such-bus";

/// True when the §8 `USS_TEST_SKIP_BOOTSTRAP` hook exists in the binary
/// under test (debug builds; see the module docs). Socket-path tests
/// early-return when false — under release they would drive a binary
/// that ignores the hook, hit the bogus bus, and exit 7 for a reason
/// these tests do not model. The dev-profile `cargo test` gate always
/// has them true.
fn debug_hook() -> bool {
    let on = cfg!(debug_assertions);
    if !on {
        eprintln!("skipped: the §8 bootstrap test hook is debug-only (release profile)");
    }
    on
}

// -- temp environment --------------------------------------------------------

/// A temp dir per test; removed on drop (fake threads keep no live
/// handles that a detach would break — an unlinked socket path is gone
/// for new connections the moment drop runs).
struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Self {
        // Keep socket paths well under the ~108-byte Unix-path limit.
        let dir = std::env::temp_dir().join(format!("uss-cli-test-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn runtime(&self) -> PathBuf {
        self.0.join("rt")
    }

    fn socket(&self) -> PathBuf {
        self.runtime().join("uss").join("ussd.sock")
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// -- the fake ussd -----------------------------------------------------------

/// What the fake does with one connection: answer with exact bytes, or
/// accept, read the request, and hold the connection silent.
enum Step {
    Reply(String),
    Silent,
}

struct FakeUssd {
    /// The request lines received so far, in order (one per connection).
    received: Arc<Mutex<Vec<String>>>,
}

impl FakeUssd {
    /// Bind `socket` SYNCHRONOUSLY (a later spawn can already connect),
    /// then serve the scripted steps in a detached background thread:
    /// one §6 connection is one request line, one response, close (the
    /// daemon's `handle_connection`). After the script, the listener
    /// drops: extra connections hit a stale socket file and get
    /// ECONNREFUSED (the row-7 `socket not ready` shape).
    fn start(socket: &Path, steps: Vec<Step>) -> Self {
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        {
            let rx = received.clone();
            thread::spawn(move || {
                for step in steps {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() {
                        return; // hung up without a request (not our client)
                    }
                    rx.lock().unwrap().push(line);
                    match step {
                        Step::Reply(bytes) => {
                            let _ = reader.get_mut().write_all(bytes.as_bytes());
                            // The stream drops here: write + close (the
                            // daemon's shutdown after its one response).
                        }
                        Step::Silent => {
                            // Hold the connection open past the client's
                            // 30 s response wait (spec §4.1).
                            thread::sleep(std::time::Duration::from_secs(35));
                            return;
                        }
                    }
                }
            });
        }
        Self { received }
    }

    fn requests(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }
}

// -- spawning the real binary -------------------------------------------------

/// Spawn the real `uss` (the binary `cargo test` just built — no stale
/// target gotcha) with a minimal, explicit environment: nothing leaks in
/// from the developer's session. `skip_bootstrap` arms the §8 test hook:
/// steps 1–4 short-circuit and the command goes straight to the fake on
/// the temp socket; with it NOT armed the bogus bus fails step 1
/// instantly (`ManagerAbsent`) — no hang, no real D-Bus.
fn uss(tmp: &Tmp, skip_bootstrap: bool, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_uss"));
    cmd.env_clear()
        .env("XDG_RUNTIME_DIR", tmp.runtime())
        .env("HOME", tmp.0.join("home"))
        .env("XDG_CONFIG_HOME", tmp.0.join("home/.config"))
        .env("DBUS_SESSION_BUS_ADDRESS", BOGUS_BUS)
        .env("USER", "tester");
    if skip_bootstrap {
        cmd.env("USS_TEST_SKIP_BOOTSTRAP", "1");
    }
    cmd.args(args).output().expect("spawn uss")
}

/// The same with the bootstrap hook armed — every socket-level test.
fn uss_faked(tmp: &Tmp, args: &[&str]) -> Output {
    uss(tmp, true, args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The full §4.1 contract in one assert: exit code, byte-exact stdout,
/// byte-exact stderr. Each stream is compared with a single trailing
/// newline stripped — every §4.3 record line and every §4.4 message ends
/// in exactly one '\n', and the empty side stays empty.
fn assert_uss(out: &Output, code: i32, stdout: &str, stderr: &str) {
    assert_eq!(out.status.code(), Some(code), "exit code");
    assert_eq!(trim_end_nl(&text(&out.stdout)), stdout, "stdout");
    assert_eq!(trim_end_nl(&text(&out.stderr)), stderr, "stderr");
}

/// The same with a per-case label (for loop-driven rows).
fn assert_uss_case(out: &Output, code: i32, stdout: &str, stderr: &str, case: &str) {
    assert_eq!(out.status.code(), Some(code), "exit code {case}");
    assert_eq!(trim_end_nl(&text(&out.stdout)), stdout, "stdout {case}");
    assert_eq!(trim_end_nl(&text(&out.stderr)), stderr, "stderr {case}");
}

fn trim_end_nl(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

/// An ok status response over the wire (§6 shape).
fn status_ok(groups_json: &str) -> Step {
    Step::Reply(format!(
        "{{\"v\":1,\"id\":1,\"ok\":true,\"result\":{{\"groups\":{groups_json}}}}}\n"
    ))
}

/// An ok empty response (a mutation's success, §6).
fn ok_empty() -> Step {
    Step::Reply("{\"v\":1,\"id\":1,\"ok\":true}\n".to_owned())
}

/// An error response (§6): stable kebab-case code + verbatim message.
fn err_resp(code: &str, message: &str) -> Step {
    Step::Reply(format!(
        "{{\"v\":1,\"id\":1,\"ok\":false,\"error\":\"{code}\",\"message\":\"{message}\"}}\n"
    ))
}

// -- §4.3 status formats -------------------------------------------------------

#[test]
fn s43_all_groups_spec_example_is_byte_exact() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("s43-all");
    let fake = FakeUssd::start(
        &tmp.socket(),
        vec![status_ok(
            r#"[{"name":"dev","members":[{"name":"foo.service","active":false}]},{"name":"vpn","members":[{"name":"openvpn.service","active":false},{"name":"wireguard.service","active":true}]}]"#,
        )],
    );
    // The §4.3 example, byte for byte: two-space indent, ` - Active` on
    // the running member only, groups as delivered (the daemon sorts
    // alphabetically — wire order here is the spec example's).
    let out = uss_faked(&tmp, &[]);
    assert_uss(
        &out,
        0,
        "dev\n  foo.service\nvpn\n  openvpn.service\n  wireguard.service - Active",
        "",
    );
    assert_eq!(
        fake.requests(),
        vec![r#"{"v":1,"id":1,"cmd":"status"}"#.to_string() + "\n"],
        "the status-all request is the §6 verbatim line"
    );
}

#[test]
fn s43_one_group() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("s43-one");
    let fake = FakeUssd::start(
        &tmp.socket(),
        vec![status_ok(
            r#"[{"name":"vpn","members":[{"name":"openvpn.service","active":true}]}]"#,
        )],
    );
    let out = uss_faked(&tmp, &["vpn"]);
    assert_uss(&out, 0, "vpn\n  openvpn.service - Active", "");
    assert_eq!(
        fake.requests(),
        vec![r#"{"v":1,"id":1,"cmd":"status","group":"vpn"}"#.to_string() + "\n"]
    );
}

#[test]
fn s43_no_groups_prints_nothing_and_exits_0() {
    if !debug_hook() {
        return;
    }

    // §4.3: "No groups → no output, exit 0."
    let tmp = Tmp::new("s43-empty");
    FakeUssd::start(&tmp.socket(), vec![status_ok("[]")]);
    let out = uss_faked(&tmp, &[]);
    assert_uss(&out, 0, "", "");
}

#[test]
fn s43_members_keep_add_order_and_groups_pass_through_in_wire_order() {
    if !debug_hook() {
        return;
    }

    // The client is a renderer: members print in wire (add) order,
    // groups in wire order — alphabetical sorting is the DAEMON's job
    // (core::status locks it against the fake); the harness locks that
    // the client neither reorders nor re-sorts what it received.
    let tmp = Tmp::new("s43-order");
    FakeUssd::start(
        &tmp.socket(),
        vec![status_ok(
            r#"[{"name":"vpn","members":[{"name":"wireguard.service","active":false},{"name":"openvpn.service","active":true}]},{"name":"dev","members":[{"name":"foo.service","active":false}]}]"#,
        )],
    );
    let out = uss_faked(&tmp, &[]);
    assert_uss(
        &out,
        0,
        "vpn\n  wireguard.service\n  openvpn.service - Active\ndev\n  foo.service",
        "",
    );
}

#[test]
fn s43_transient_states_print_bare() {
    if !debug_hook() {
        return;
    }

    // The wire carries the boolean (§6): every non-active state
    // (`activating`/`deactivating`/`failed` — the daemon maps them all to
    // false) prints bare, no marker.
    let tmp = Tmp::new("s43-transient");
    FakeUssd::start(
        &tmp.socket(),
        vec![status_ok(
            r#"[{"name":"dev","members":[{"name":"a.service","active":false},{"name":"b.service","active":true}]}]"#,
        )],
    );
    let out = uss_faked(&tmp, &[]);
    assert_uss(&out, 0, "dev\n  a.service\n  b.service - Active", "");
}

// -- row 0: mutations print nothing; requests ride the §6 shapes ---------------

#[test]
fn add_success_prints_nothing_and_normalizes_over_the_wire() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("add-ok");
    let fake = FakeUssd::start(&tmp.socket(), vec![ok_empty()]);
    let out = uss_faked(&tmp, &["dev", "add", "foo"]);
    // §4.1: stdout carries status data only — a mutation prints nothing.
    assert_uss(&out, 0, "", "");
    assert_eq!(
        fake.requests(),
        vec![
            r#"{"v":1,"id":1,"cmd":"add","group":"dev","service":"foo.service"}"#.to_string()
                + "\n"
        ],
        "the bare name rides the wire in the normalized §10 form"
    );
}

#[test]
fn every_mutation_verb_prints_nothing_on_success() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("mutations-ok");
    let fake = FakeUssd::start(
        &tmp.socket(),
        vec![ok_empty(), ok_empty(), ok_empty(), ok_empty()],
    );
    for args in [
        ["dev", "remove", "foo"],
        ["dev", "start", "foo"],
        ["dev", "stop", "foo.service"],
        ["dev", "add", "bar"],
    ] {
        let out = uss_faked(&tmp, &args);
        assert_uss_case(&out, 0, "", "", "{args:?}");
    }
    assert_eq!(
        fake.requests(),
        vec![
            r#"{"v":1,"id":1,"cmd":"remove","group":"dev","service":"foo.service"}"#,
            r#"{"v":1,"id":1,"cmd":"start","group":"dev","service":"foo.service"}"#,
            r#"{"v":1,"id":1,"cmd":"stop","group":"dev","service":"foo.service"}"#,
            r#"{"v":1,"id":1,"cmd":"add","group":"dev","service":"bar.service"}"#,
        ]
        .into_iter()
        .map(|s| s.to_string() + "\n")
        .collect::<Vec<_>>()
    );
}

// -- row 1: usage (client-side, before ANY socket traffic) ---------------------

#[test]
fn row1_usage_and_name_errors_exit_1_before_any_socket_traffic() {
    // No fake runs at all: were any of these to fall through to the
    // socket path, the exit code would be 7 (`socket not ready` after
    // the 2 s window) — the byte-exact 1s below ARE the proof nothing
    // reached the socket.
    let tmp = Tmp::new("row1");
    for (args, stderr) in [
        (
            vec!["vpn", "add", "foo.target"],
            "uss: invalid service name: 'foo.target' (use 'name' or 'name.service')",
        ),
        (
            vec!["vpn", "stop", "foo.target.wants"],
            "uss: invalid service name: 'foo.target.wants' (use 'name' or 'name.service')",
        ),
        (vec!["bad group"], "uss: invalid group name: 'bad group'"),
        (
            vec!["bad/group", "add", "foo"],
            "uss: invalid group name: 'bad/group'",
        ),
        (
            vec!["vpn", "start"],
            "uss: a service is required (usage: uss vpn start <service>)",
        ),
        (
            vec!["vpn", "remove"],
            "uss: a service is required (usage: uss vpn remove <service>)",
        ),
    ] {
        let out = uss_faked(&tmp, &args);
        assert_uss_case(&out, 1, "", stderr, "{args:?}");
    }
}

#[test]
fn row1_clap_rejections_exit_1() {
    // Spec §4.4 row 1: bad verb values / extra positionals are clap's
    // own messages — what the harness locks is the exit code (1, not
    // clap's default 2) and the stdout/stderr split.
    let tmp = Tmp::new("row1-clap");
    for args in [
        vec!["vpn", "fly", "foo"],
        vec!["vpn", "start", "foo", "extra"],
    ] {
        let out = uss_faked(&tmp, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?} stdout");
        let err = text(&out.stderr);
        // clap's own text (it may not name the program at all — e.g. the
        // invalid-value case names only the position); the harness locks
        // the class, not clap's wording.
        assert!(err.starts_with("error: "), "{args:?}: {err}");
    }
}

#[test]
fn help_and_version_exit_0_on_stdout() {
    // §4.1 "standard": success-flavored — exit 0, stdout (the exit-1
    // remap must not catch them).
    let tmp = Tmp::new("help");
    let out = uss_faked(&tmp, &["--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("switch between mutually exclusive user services"));
    assert!(out.stderr.is_empty());

    let out = uss_faked(&tmp, &["--version"]);
    assert_eq!(out.status.code(), Some(0));
    // clap's version render already ends in '\n' and main println-s —
    // trim_end to the payload, which is byte-exact after that.
    assert_eq!(text(&out.stdout).trim_end(), "uss 0.1.0");
    assert!(out.stderr.is_empty());
}

// -- rows 2–6, 8: daemon-reported errors (code -> row, message verbatim) -------

#[test]
fn row2_unknown_group() {
    if !debug_hook() {
        return;
    }

    // Row 2 is "any command naming a group that does not exist" —
    // a status query and a mutation share the daemon error.
    let tmp = Tmp::new("row2");
    FakeUssd::start(
        &tmp.socket(),
        vec![
            err_resp("unknown-group", "no such group: nope"),
            err_resp("unknown-group", "no such group: nope"),
        ],
    );
    let out = uss_faked(&tmp, &["nope"]);
    assert_uss(&out, 2, "", "uss: no such group: nope");
    let out = uss_faked(&tmp, &["nope", "stop", "foo"]);
    assert_uss(&out, 2, "", "uss: no such group: nope");
}

#[test]
fn row3_not_a_member() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("row3");
    FakeUssd::start(
        &tmp.socket(),
        vec![err_resp(
            "not-a-member",
            "ghost.service is not in group vpn",
        )],
    );
    let out = uss_faked(&tmp, &["vpn", "start", "ghost"]);
    assert_uss(&out, 3, "", "uss: ghost.service is not in group vpn");
}

#[test]
fn row4_already_in_another_group() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("row4");
    FakeUssd::start(
        &tmp.socket(),
        vec![err_resp(
            "service-in-other-group",
            "wireguard.service already belongs to group vpn",
        )],
    );
    let out = uss_faked(&tmp, &["dev", "add", "wireguard"]);
    assert_uss(
        &out,
        4,
        "",
        "uss: wireguard.service already belongs to group vpn",
    );
}

#[test]
fn row5_not_found_and_masked() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("row5");
    FakeUssd::start(
        &tmp.socket(),
        vec![
            err_resp("unit-not-loadable", "service ghost.service not found"),
            err_resp("unit-masked", "service foo.service is masked"),
        ],
    );
    let out = uss_faked(&tmp, &["dev", "add", "ghost"]);
    assert_uss(&out, 5, "", "uss: service ghost.service not found");
    let out = uss_faked(&tmp, &["dev", "start", "foo"]);
    assert_uss(&out, 5, "", "uss: service foo.service is masked");
}

#[test]
fn row6_op_failed_method_rejected_carries_the_dbus_error() {
    if !debug_hook() {
        return;
    }

    // The §6 `op-failed` message is the row-6 `Rejected` template verbatim
    // (the daemon sends `Error::OpFailed(...)`'s Display unchanged).
    let tmp = Tmp::new("row6-rej");
    FakeUssd::start(
        &tmp.socket(),
        vec![
            err_resp(
                "op-failed",
                "failed to stop foo.service: org.freedesktop.systemd1.Transactional (A stop job is queued for the unit.)",
            ),
            err_resp(
                "op-failed",
                "failed to start foo.service: org.freedesktop.systemd1.NoSuchUnit (Unit foo.service not loaded.)",
            ),
        ],
    );
    let out = uss_faked(&tmp, &["dev", "stop", "foo"]);
    assert_uss(
        &out,
        6,
        "",
        "uss: failed to stop foo.service: org.freedesktop.systemd1.Transactional (A stop job is queued for the unit.)",
    );
    let out = uss_faked(&tmp, &["dev", "start", "foo"]);
    assert_uss(
        &out,
        6,
        "",
        "uss: failed to start foo.service: org.freedesktop.systemd1.NoSuchUnit (Unit foo.service not loaded.)",
    );
}

#[test]
fn row6_op_failed_bad_job_results() {
    if !debug_hook() {
        return;
    }

    // Every unsuccessful JobRemoved result the §4.2/§6 rules surface
    // (failed, canceled, timeout, dependency) rides the `job <result>`
    // template.
    for result in ["failed", "canceled", "timeout", "dependency"] {
        let tmp = Tmp::new(&format!("row6-job-{result}"));
        FakeUssd::start(
            &tmp.socket(),
            vec![err_resp(
                "op-failed",
                &format!("failed to start foo.service: job {result}"),
            )],
        );
        let out = uss_faked(&tmp, &["dev", "start", "foo"]);
        assert_uss(
            &out,
            6,
            "",
            &format!("uss: failed to start foo.service: job {result}"),
        );
    }
}

#[test]
fn row6_op_failed_conflicting_job() {
    if !debug_hook() {
        return;
    }

    // The §4.2 conflicting-job rule exhausted during the stop phase of
    // a switch: the target's `start` request fails on another member's
    // stop.
    let tmp = Tmp::new("row6-conflict");
    FakeUssd::start(
        &tmp.socket(),
        vec![err_resp(
            "op-failed",
            "failed to stop foo.service: conflicting job",
        )],
    );
    let out = uss_faked(&tmp, &["dev", "start", "bar"]);
    assert_uss(
        &out,
        6,
        "",
        "uss: failed to stop foo.service: conflicting job",
    );
}

#[test]
fn row8_daemon_internal_error_prints_the_message_verbatim() {
    if !debug_hook() {
        return;
    }

    // The real daemon's `internal` wire message already IS the row-8
    // template (`Error::Internal`'s Display) — uss maps the exit code
    // from the code and prints the message as-is.
    let tmp = Tmp::new("row8-internal");
    FakeUssd::start(
        &tmp.socket(),
        vec![err_resp(
            "internal",
            "daemon error: state write failed: disk full",
        )],
    );
    let out = uss_faked(&tmp, &["dev", "add", "foo"]);
    assert_uss(
        &out,
        8,
        "",
        "uss: daemon error: state write failed: disk full",
    );
}

#[test]
fn row8_daemon_protocol_error_code_maps_to_8() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("row8-proto");
    FakeUssd::start(
        &tmp.socket(),
        vec![err_resp(
            "protocol",
            "malformed request shape (group/service do not match the command)",
        )],
    );
    let out = uss_faked(&tmp, &[]);
    assert_uss(
        &out,
        8,
        "",
        "uss: malformed request shape (group/service do not match the command)",
    );
}

#[test]
fn row8_response_version_mismatch_is_exit_8() {
    if !debug_hook() {
        return;
    }

    // §6 versioning: uss rejects a response version it doesn't know
    // (the client-side template carries the row-8 `daemon error:`
    // wrapper).
    let tmp = Tmp::new("row8-version");
    FakeUssd::start(
        &tmp.socket(),
        vec![Step::Reply(
            "{\"v\":2,\"id\":1,\"ok\":true,\"result\":{\"groups\":[]}}\n".to_owned(),
        )],
    );
    let out = uss_faked(&tmp, &[]);
    assert_uss(
        &out,
        8,
        "",
        "uss: daemon error: unsupported protocol version 2 (expected 1)",
    );
}

#[test]
fn row8_garbage_response_is_a_protocol_error() {
    if !debug_hook() {
        return;
    }

    let tmp = Tmp::new("row8-garbage");
    FakeUssd::start(
        &tmp.socket(),
        vec![Step::Reply("not json at all\n".to_owned())],
    );
    let out = uss_faked(&tmp, &[]);
    assert_eq!(out.status.code(), Some(8));
    assert!(out.stdout.is_empty());
    let err = text(&out.stderr);
    assert!(
        err.starts_with("uss: daemon error: malformed message: "),
        "{err}"
    );
}

// -- row 7: the environment class ------------------------------------------------

#[test]
fn row7_user_manager_absent_names_the_linger_command() {
    // The hook is NOT armed (plain `uss`): §8 step 1 hits the bogus bus,
    // `ManagerAbsent` maps to the linger row — with $USER interpolated.
    let tmp = Tmp::new("row7-manager");
    let out = uss(&tmp, false, &[]);
    assert_uss(
        &out,
        7,
        "",
        "uss: user manager not running — log in to a session or run: loginctl enable-linger tester",
    );
}

#[test]
fn row7_missing_xdg_runtime_dir_is_the_linger_row() {
    // §8 step 1: no `$XDG_RUNTIME_DIR` means no user session — the same
    // row, before any bus touch (so no fake, nothing bound).
    let tmp = Tmp::new("row7-norun");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_uss"));
    cmd.env_clear()
        .env("DBUS_SESSION_BUS_ADDRESS", BOGUS_BUS)
        .env("USER", "tester")
        .args(["vpn"]);
    let out = cmd.output().expect("spawn uss");
    assert_uss(
        &out,
        7,
        "",
        "uss: user manager not running — log in to a session or run: loginctl enable-linger tester",
    );
    // The temp tree is untouched — nothing got created.
    assert!(!tmp.runtime().join("uss").exists());
}

#[test]
fn row7_socket_never_appears_is_unavailable_after_the_retry_window() {
    if !debug_hook() {
        return;
    }

    // The hook IS armed but no fake exists: step 5's 2 s window (§8)
    // expires as `ussd unavailable (socket not ready)`.
    let tmp = Tmp::new("row7-nosock");
    let started = std::time::Instant::now();
    let out = uss_faked(&tmp, &[]);
    assert_uss(&out, 7, "", "uss: ussd unavailable (socket not ready)");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(1900)
            && elapsed < std::time::Duration::from_secs(10),
        "the retry window is the ~2 s budget, not a hang: {elapsed:?}"
    );
}

#[test]
fn row7_silent_daemon_times_out_after_the_full_30s() {
    if !debug_hook() {
        return;
    }

    // The one slow test on purpose (see the module docs): the binary's
    // own RESPONSE_TIMEOUT must be the spec's 30 s and the reason cell
    // byte-exact. §4.1: meanwhile the operation continues inside ussd.
    let tmp = Tmp::new("row7-timeout");
    let fake = FakeUssd::start(&tmp.socket(), vec![Step::Silent]);
    let started = std::time::Instant::now();
    let out = uss_faked(&tmp, &[]);
    let elapsed = started.elapsed();
    assert_uss(
        &out,
        7,
        "",
        "uss: ussd unavailable (no response within 30s)",
    );
    assert!(
        elapsed >= std::time::Duration::from_secs(29),
        "the wait is the full 30 s: {elapsed:?}"
    );
    assert_eq!(
        fake.requests(),
        vec![r#"{"v":1,"id":1,"cmd":"status"}"#.to_string() + "\n"]
    );
}
