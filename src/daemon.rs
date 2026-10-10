//! The ussd daemon (spec §6/§9): Unix socket server + lifecycle.
//!
//! `run` wires the pieces the earlier slices built:
//! - the socket at `$XDG_RUNTIME_DIR/uss/ussd.sock` — parent dir `0700`,
//!   socket `0600`, created on startup, removed on clean shutdown (no
//!   systemd socket activation);
//! - one JSON object per line, `"v": 1` enforced, exactly one response
//!   per connection;
//! - one global command lock, strictly sequential (status takes it too);
//! - the bus-drop policy: ussd does not exit — backoff → `connect` →
//!   `subscribe` → re-sync before signal reactions re-enable;
//! - SIGTERM/SIGINT (the binary wires the signals here) → clean
//!   shutdown, exit 0 — a clean stop is not a failure, so the unit's
//!   `Restart=on-failure` restarts real failures only.
//!
//! **The command lock is structural:** the whole daemon runs as one task
//! — the serve loop is the lock. The seam's futures are `!Send` (the
//! AFIT tradeoff documented at the seam), so no spawned task may await a
//! seam call; single-task multiplexing is both the Send-safe shape and
//! the spec's "strictly sequential" rule (a reaction holding the loop
//! while it stops a member is exactly the serialization the spec wants).
//!
//! Journal lines go to stderr — the systemd unit's journal sink.
//!
//! Runtime state authority: the in-memory `State` is authoritative for
//! the daemon's whole lifetime — the file is the persistence substrate
//! (spec §5: only ussd writes), and it is never re-read at runtime (a
//! manual edit of `groups.json` while ussd runs does not take effect
//! until the next daemon start).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::core::{self, State};
use crate::error::{Error, ErrorCode, OpError};
use crate::jobs::JobBook;
use crate::protocol::{self, Cmd, Request, Response};
use crate::systemdctl::{SystemdCtl, UnitStateChanged};
use crate::watchdog::Watchdog;

/// One request line's ceiling (spec §6: one JSON object per line; a
/// realistic request is well under 1 KiB).
const MAX_REQUEST_LINE: usize = 1024 * 1024;

/// Default reconnect budget: 1+2+4+8+16 s of backoff ≈ 31 s — a manager
/// restart gets the window; a truly dead manager exits the daemon.
pub const DEFAULT_ABSENT_BUDGET: u32 = 5;

/// The daemon's run configuration (spec §6/§9).
#[derive(Debug, Clone)]
pub struct Config {
    /// `$XDG_CONFIG_HOME/uss/groups.json` (spec §5).
    pub state_path: PathBuf,
    /// `$XDG_RUNTIME_DIR/uss/ussd.sock` (spec §6).
    pub socket_path: PathBuf,
    /// Consecutive `ManagerAbsent` reconnect attempts tolerated before
    /// the daemon exits [`Exit::ManagerDied`].
    pub absent_budget: u32,
}

/// How the daemon's [`run`] finished (the binary maps it to a process
/// exit code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Clean stop (SIGTERM/SIGINT): socket removed — exit 0 (a clean
    /// stop is not a failure, §9).
    Clean,
    /// The user manager stayed absent beyond the budget: exit 1 — the
    /// death of the user manager ends ussd (spec §6); the next `uss`
    /// command bootstraps (§8).
    ManagerDied,
    /// Startup aborted (a clear journal line was already emitted): exit 1.
    Aborted { reason: String },
}

/// The outcome of the reconnect sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reconnect {
    /// Re-established and re-synced.
    Ok,
    /// The budget ran out: manager dead.
    ManagerDied,
    /// Shutdown requested during the backoff.
    ShutDown,
}

/// One event the serve loop saw.
enum Event {
    /// A member `ActiveState` edge (spec §7 detection).
    Edge(UnitStateChanged),
    /// A unit-file change (UnitFilesChanged / Reloading(false), spec §7).
    FilesChanged,
    /// The edge stream lagged (the receiver's own events dropped).
    EdgeLagged(u64),
    /// The files stream lagged.
    FilesLagged(u64),
    /// A client connection (handled inline — the loop is the lock).
    Connection(UnixStream),
    /// The bus connection dropped: the full reconnect sequence.
    BusDrop,
    /// Shutdown requested (SIGTERM/SIGINT).
    ShutDown,
}

/// Run the daemon (spec §6/§9).
///
/// `ctl` is the [`SystemdCtl`] seam (the real zbus backend in the binary,
/// the fake in tests); `shutdown` flips to `true` for a clean stop (the
/// binary wires SIGTERM/SIGINT to it). Returns the classified exit; the
/// journal lines (stderr) are emitted along the way.
///
/// Not `Send` (it awaits the `!Send` seam) — run it on the current-thread
/// runtime, or inline (`tokio::join!` in tests).
pub async fn run<C: SystemdCtl>(ctl: &C, cfg: Config, mut shutdown: watch::Receiver<bool>) -> Exit {
    // 1. State: a corrupt file is a hard startup error (spec §5) — a clear
    //    journal line; systemd's start-rate limit then takes the unit to
    //    `failed`, so no loop.
    let state = match State::load(&cfg.state_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ussd: refusing to start: {e}");
            return Exit::Aborted {
                reason: format!("refusing to start: {e}"),
            };
        }
    };

    // 2. Bus: connect + subscribe + re-sync BEFORE signal reactions are
    //    enabled (spec §3/§7).
    if let Err(e) = ctl.connect().await {
        eprintln!("ussd: user manager unavailable at startup ({e}) — exiting");
        return Exit::Aborted {
            reason: format!("user manager unavailable at startup: {e}"),
        };
    }
    {
        if let Err(e) = Watchdog::resync(ctl, &state.groups).await {
            eprintln!("ussd: initial re-sync failed: {e} — exiting");
            return Exit::Aborted {
                reason: format!("initial re-sync failed: {e}"),
            };
        }
    }
    let mut state = state;
    let mut book = JobBook::default();
    let mut watchdog = Watchdog::new();
    let mut edge_rx = ctl.unit_state_changed();
    let mut files_rx = ctl.unit_files_changed();

    // 3. Socket (spec §6): parent dir 0700, socket 0600; a stale socket
    //    from an unclean exit is probed and removed, a live one refuses
    //    the start.
    let listener = match setup_socket(&cfg.socket_path) {
        Ok(l) => l,
        Err(reason) => {
            eprintln!("ussd: {reason} — exiting");
            return Exit::Aborted { reason };
        }
    };

    // 4. The serve loop: one task, strictly sequential (the loop IS the
    //    command lock). A bus drop or a manager-death error from a
    //    command triggers the reconnect sequence (below) before the next
    //    iteration.
    let mut needs_reconnect = false;
    loop {
        if needs_reconnect {
            match reconnect(ctl, &state.groups, cfg.absent_budget, &shutdown).await {
                Reconnect::Ok => {
                    // The (possibly new) connection's streams: re-enable
                    // signal reactions only after the re-sync above.
                    edge_rx = ctl.unit_state_changed();
                    files_rx = ctl.unit_files_changed();
                }
                Reconnect::ManagerDied => return Exit::ManagerDied,
                Reconnect::ShutDown => break,
            }
            needs_reconnect = false;
        }

        let event = {
            let edge = edge_rx.recv();
            let files = files_rx.recv();
            let accept = listener.accept();
            let dropped = ctl.connection_lost();
            let sig = shutdown.wait_for(|s| *s);
            tokio::select! {
                res = edge => match res {
                    Ok(e) => Event::Edge(e),
                    Err(RecvError::Lagged(n)) => Event::EdgeLagged(n),
                    Err(RecvError::Closed) => Event::BusDrop,
                },
                res = files => match res {
                    Ok(()) => Event::FilesChanged,
                    Err(RecvError::Lagged(n)) => Event::FilesLagged(n),
                    Err(RecvError::Closed) => Event::BusDrop,
                },
                res = accept => {
                    let (socket, _) = match res {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("ussd: accept failed: {e}");
                            continue;
                        }
                    };
                    Event::Connection(socket)
                }
                _ = dropped => Event::BusDrop,
                _ = sig => Event::ShutDown,
            }
        };

        match event {
            Event::Edge(edge) => {
                watchdog.push_edge(edge);
                // Process this edge against the live state (the §7
                // reaction, incl. its stops) — sequential with commands.
                if let Err(e) = watchdog.process_next(ctl, &state.groups, &mut book).await {
                    // A failed live read drops the edge (the doc on
                    // process_next): the next re-sync covers it.
                    eprintln!("ussd: watchdog edge dropped (live read failed): {e}");
                }
            }
            Event::FilesChanged => {
                if let Err(e) = Watchdog::revalidate_unit_files(ctl, &state.groups).await {
                    eprintln!("ussd: unit-file re-validation failed: {e}");
                }
            }
            Event::EdgeLagged(n) => {
                // Receiver lag drops the receiver's own events (broadcast
                // semantics): the spec's answer is a re-sync, and the
                // lagging receiver is stale — re-subscribe.
                eprintln!("ussd: edge stream lagged ({n} events lost); re-syncing");
                if let Err(e) = Watchdog::resync(ctl, &state.groups).await {
                    eprintln!("ussd: re-sync after lag failed: {e}");
                }
                edge_rx = ctl.unit_state_changed();
            }
            Event::FilesLagged(n) => {
                // A lost re-validation is best-effort (spec §7): the next
                // event, a re-sync, or the next `start`/`add` covers it.
                eprintln!("ussd: unit-file stream lagged ({n}); skipping");
            }
            Event::Connection(mut socket) => {
                let (_response, manager_gone) =
                    handle_connection(ctl, &mut socket, &mut state, &mut book).await;
                if manager_gone {
                    // The call failed with a name-gone/no-reply error:
                    // the manager may have DIED on a still-open socket
                    // (which does not fire `connection_lost`). Re-verify;
                    // a healthy connection makes this a no-op early
                    // return in the backend.
                    if ctl.connect().await.is_err() {
                        needs_reconnect = true;
                    }
                }
            }
            Event::BusDrop => {
                // ussd does not exit on a bus drop (spec §6).
                needs_reconnect = true;
            }
            Event::ShutDown => break,
        }
    }

    // Clean stop: remove the socket (spec §6) — a clean stop is not a
    // failure (spec §9). The loop only yields at select points, so no
    // command or reaction is mid-flight when the signal lands (a signal
    // arriving during a reaction is seen by the next select iteration,
    // after the reaction completes).
    if let Err(e) = std::fs::remove_file(&cfg.socket_path) {
        eprintln!("ussd: could not remove {:?}: {e}", cfg.socket_path);
    }
    Exit::Clean
}

/// The reconnect sequence (spec §6): backoff → `connect` →
/// `subscribe` → re-sync (all members' live state) before signal
/// reactions re-enable. A manager that stays absent beyond the budget
/// ends the daemon — the death of the user manager ends ussd (spec §6).
async fn reconnect<C: SystemdCtl>(
    ctl: &C,
    groups: &crate::state::Groups,
    budget: u32,
    shutdown: &watch::Receiver<bool>,
) -> Reconnect {
    let mut delay = Duration::from_secs(1);
    let mut attempts = 0;
    loop {
        match ctl.connect().await {
            Ok(()) => {
                // re-Subscribe + re-read all members' ActiveState before
                // re-enabling signal reactions (spec §7).
                if let Err(e) = Watchdog::resync(ctl, groups).await {
                    eprintln!("ussd: re-sync after reconnect failed: {e}");
                }
                return Reconnect::Ok;
            }
            Err(_) => {
                attempts += 1;
                if attempts > budget {
                    eprintln!(
                        "ussd: user manager still absent after {attempts} reconnect attempts \
                         — exiting (the death of the user manager ends ussd, spec §6; the \
                         next uss command bootstraps, §8)"
                    );
                    return Reconnect::ManagerDied;
                }
                eprintln!(
                    "ussd: user manager unavailable (attempt {attempts}), retrying in {delay:?}"
                );
                let mut shutdown_rx = shutdown.clone();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.wait_for(|s| *s) => return Reconnect::ShutDown,
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// One connection: read one request line, dispatch (the loop is the
/// command lock — no lock needed here), write exactly one response,
/// close (spec §6). Returns whether the dispatch hit a manager-death
/// error (the caller re-verifies the manager).
async fn handle_connection<C: SystemdCtl>(
    ctl: &C,
    socket: &mut UnixStream,
    state: &mut State,
    book: &mut JobBook,
) -> (Response, bool) {
    // Bounded raw reads: the `MAX_REQUEST_LINE` ceiling is enforced
    // MID-STREAM — `read_until` accumulates everything until its
    // delimiter, so a newline-less payload would grow the heap unbounded
    // (a same-user client could OOM the daemon). One request per
    // connection (spec §6): read until the first line (or ceiling/EOF).
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buf.len() > MAX_REQUEST_LINE {
            break; // ceiling: refused below as a protocol error
        }
        let n = match socket.read(&mut chunk).await {
            Ok(n) => n,
            Err(_) => return (no_request(), false), // the peer hung up
        };
        if n == 0 {
            break; // EOF
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') {
            break; // a complete line has arrived
        }
    }
    if buf.is_empty() {
        return (no_request(), false); // an empty connection gets no response
    }
    // The request is the first line; any bytes after the newline are
    // ignored (one request per connection).
    let line_end = buf.iter().position(|b| *b == b'\n').unwrap_or(buf.len());
    let line = String::from_utf8_lossy(&buf[..line_end]);
    let (response, manager_gone) = match protocol::parse_request(line.trim()) {
        Ok(req) => dispatch(ctl, req, state, book).await,
        Err(e) => (
            Response::err(0, ErrorCode::Protocol, format!("protocol: {e}")),
            false,
        ),
    };
    let encoded = protocol::encode(&response);
    let _ = socket.write_all(encoded.as_bytes()).await;
    let _ = socket.shutdown().await;
    (response, manager_gone)
}

/// The no-response case (empty/hung-up connection) — the response is
/// never written; the shape is irrelevant, kept for the tuple.
fn no_request() -> Response {
    Response::err(0, ErrorCode::Protocol, String::new())
}

/// Dispatch one parsed request (spec §6: strictly sequential — the serve
/// loop runs one dispatch at a time). Returns the response and whether a
/// manager-death error was hit (the caller re-verifies the manager).
async fn dispatch<C: SystemdCtl>(
    ctl: &C,
    req: Request,
    state: &mut State,
    book: &mut JobBook,
) -> (Response, bool) {
    let mut manager_gone = false;
    let response = match (&req.group, &req.service) {
        (None, None) if req.cmd == Cmd::Status => {
            match core::status(ctl, &state.groups, None).await {
                Ok(result) => Response::ok(req.id, result),
                Err(e) => err_response(req.id, &e),
            }
        }
        (Some(group), None) if req.cmd == Cmd::Status => {
            match core::status(ctl, &state.groups, Some(group)).await {
                Ok(result) => Response::ok(req.id, result),
                Err(e) => err_response(req.id, &e),
            }
        }
        (Some(group), Some(service))
            if matches!(req.cmd, Cmd::Add | Cmd::Remove | Cmd::Start | Cmd::Stop) =>
        {
            let result = match req.cmd {
                Cmd::Add => core::add(ctl, group, service, state).await,
                Cmd::Remove => core::remove(ctl, group, service, book, state).await,
                Cmd::Start => core::start(ctl, group, service, book, state).await,
                Cmd::Stop => core::stop(ctl, group, service, book, state).await,
                // The outer guard already rejected `status` with a
                // group/service shape — a defensive fallback, not a
                // panic (the daemon must not die on client input).
                Cmd::Status => {
                    return (
                        Response::err(
                            req.id,
                            ErrorCode::Protocol,
                            "malformed request shape (group/service do not match the command)",
                        ),
                        false,
                    );
                }
            };
            match result {
                Ok(()) => Response::ok_empty(req.id),
                Err(e) => {
                    manager_gone = pokes_reconnect(&e);
                    err_response(req.id, &e)
                }
            }
        }
        // group/service shapes that do not match the command.
        _ => Response::err(
            req.id,
            ErrorCode::Protocol,
            "malformed request shape (group/service do not match the command)",
        ),
    };
    (response, manager_gone)
}

/// A rejection whose D-Bus error name says the manager name is gone (or
/// the manager never replied) — the manager-death signal that does NOT
/// close the socket, so `connection_lost` never fires for it.
fn pokes_reconnect(e: &Error) -> bool {
    matches!(
        e,
        Error::OpFailed(OpError::Rejected { dbus_name, .. })
            if dbus_name
                == "org.freedesktop.DBus.Error.NameHasNoOwner"
                || dbus_name == "org.freedesktop.DBus.Error.NoReply"
    )
}

/// The error response for a failed command (spec §6): the stable code +
/// the §4.4 message verbatim. Client-side error classes (usage,
/// environment) carry no stable code — they happen before any socket
/// traffic; if one reached the daemon it is an unexpected shape, so the
/// wire code is `internal` (the message is still the §4.4 template the
/// client prints).
fn err_response(id: u64, e: &Error) -> Response {
    let code = e.code().unwrap_or(ErrorCode::Internal);
    Response::err(id, code, e.to_string())
}

/// Create the socket (spec §6): parent dir `0700`, socket `0600`. A
/// pre-existing socket is probed: a live one means another ussd is
/// running (refuse); a dead one is stale and removed.
fn setup_socket(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create socket dir {dir:?}: {e}"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot set 0700 on socket dir {dir:?}: {e}"))?;
    }
    if path.exists() {
        let live = std::os::unix::net::UnixStream::connect(path).is_ok();
        if live {
            return Err(format!(
                "{} is already owned by a running ussd",
                path.display()
            ));
        }
        std::fs::remove_file(path)
            .map_err(|e| format!("cannot remove stale socket {}: {e}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    // The 0700 parent dir already gates access; the 0600 socket is the
    // spec's stated mode.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot set 0600 on socket {}: {e}", path.display()))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MemberStatus, parse_response};
    use crate::state::STATE_VERSION;
    use crate::systemdctl::{
        ActiveState, CtlError, FakeSystemdCtl, JobOrigin, JobResult, LoadState, UnitState,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// std-based temp dir (same pattern as the core tests; no tempfile
    /// dep).
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("ussd-daemon-test-{}-{tag}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("config").join("uss").join("groups.json")
        }

        fn socket_path(&self) -> PathBuf {
            self.0.join("runtime").join("uss").join("ussd.sock")
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn spawn_cfg(tmp: &Tmp, absent_budget: u32) -> Config {
        Config {
            state_path: tmp.state_path(),
            socket_path: tmp.socket_path(),
            absent_budget,
        }
    }

    fn state_with_vpn(members: &[&str]) -> String {
        serde_json::json!({
            "version": STATE_VERSION,
            "groups": { "vpn": members },
        })
        .to_string()
    }

    /// A hand-rolled client (like uss will be): one request line, one
    /// response line. Connects with retries — ussd creates the socket
    /// during startup (spec §8 step 5: the client retries up to 2 s).
    async fn client_request(socket: &Path, line: &str) -> protocol::Response {
        let mut stream = UnixStream::connect(socket)
            .await
            .expect("the daemon's socket should be up");
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        BufReader::new(&mut stream)
            .read_until(b'\n', &mut buf)
            .await
            .unwrap();
        parse_response(std::str::from_utf8(&buf).unwrap()).unwrap()
    }

    /// Poll until the fake has a queued stop job for `unit`, then settle
    /// it `done` and apply the live-state effect (the fake never mutates
    /// state on its own).
    async fn settle_stop(fake: &FakeSystemdCtl, unit: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match fake.pending_job(unit) {
                Some(id) => {
                    fake.settle_job(id, JobResult::Done);
                    fake.set_inactive(unit);
                    return;
                }
                None => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "no stop job for {unit} was issued within the deadline"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    /// Drive the daemon and one client request together (the daemon's
    /// future is `!Send` — `join!` on the current-thread runtime is the
    /// Send-free multiplexing), then shut the daemon down cleanly.
    async fn drive(fake: FakeSystemdCtl, cfg: Config, request: &str) -> (Exit, protocol::Response) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let socket_path = cfg.socket_path.clone();
        let daemon = run(&fake, cfg, shutdown_rx);
        let client = async {
            // Give the daemon a few turns to reach the socket bind.
            for _ in 0..200 {
                if UnixStream::connect(&socket_path).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let resp = client_request(&socket_path, request).await;
            let _ = shutdown_tx.send(true);
            resp
        };
        tokio::join!(daemon, client)
    }

    // -- startup / socket (acceptance, fake side) ---------------------------

    #[tokio::test]
    async fn status_all_serves_the_live_active_flags() {
        let tmp = Tmp::new("status");
        let fake = FakeSystemdCtl::new();
        fake.set_active("openvpn.service");
        fake.set_inactive("wireguard.service");
        fs::create_dir_all(tmp.state_path().parent().unwrap()).unwrap();
        fs::write(
            tmp.state_path(),
            state_with_vpn(&["openvpn.service", "wireguard.service"]),
        )
        .unwrap();

        let (exit, resp) =
            drive(fake, spawn_cfg(&tmp, 2), r#"{"v":1,"id":1,"cmd":"status"}"#).await;
        assert_eq!(exit, Exit::Clean, "a clean stop is not a failure");
        assert!(resp.ok, "{resp:?}");
        let result = resp.result.unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].name, "vpn");
        assert_eq!(
            result.groups[0].members,
            vec![
                MemberStatus {
                    name: "openvpn.service".into(),
                    active: true
                },
                MemberStatus {
                    name: "wireguard.service".into(),
                    active: false
                }
            ]
        );
        assert!(
            !tmp.socket_path().exists(),
            "a clean stop removes the socket"
        );
    }

    #[tokio::test]
    async fn socket_and_parent_dir_get_the_spec_modes() {
        let tmp = Tmp::new("modes");
        let fake = FakeSystemdCtl::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let daemon = run(&fake, spawn_cfg(&tmp, 2), shutdown_rx);
        // Reach the socket bind, then check the modes while it serves.
        let (exit, modes) = tokio::join!(daemon, async {
            for _ in 0..200 {
                if tmp.socket_path().exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let socket_mode = fs::metadata(tmp.socket_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let dir_mode = fs::metadata(tmp.socket_path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let _ = shutdown_tx.send(true);
            (socket_mode, dir_mode)
        });
        assert!(matches!(exit, Exit::Clean));
        assert_eq!(modes.0, 0o600, "socket mode");
        assert_eq!(modes.1, 0o700, "parent dir mode");
        assert!(
            !tmp.socket_path().exists(),
            "a clean stop removes the socket"
        );
    }

    #[tokio::test]
    async fn a_second_instance_refuses_a_live_socket() {
        let tmp = Tmp::new("second");
        let fake = FakeSystemdCtl::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let daemon = run(&fake, spawn_cfg(&tmp, 2), shutdown_rx);
        let (exit1, exit2) = tokio::join!(daemon, async {
            // Wait for the first instance's socket.
            for _ in 0..200 {
                if tmp.socket_path().exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The second run: the probe finds a live listener → Aborted.
            let cfg2 = Config {
                state_path: tmp.0.join("config2").join("uss").join("groups.json"),
                socket_path: tmp.socket_path(),
                absent_budget: 2,
            };
            let (_, rx2) = watch::channel(false);
            let fake2 = FakeSystemdCtl::new();
            let exit2 = run(&fake2, cfg2, rx2).await;
            let _ = shutdown_tx.send(true);
            exit2
        });
        assert!(matches!(exit1, Exit::Clean), "{exit1:?}");
        assert!(
            matches!(exit2, Exit::Aborted { ref reason } if reason.contains("already owned")),
            "{exit2:?}"
        );
    }

    #[tokio::test]
    async fn corrupt_state_file_refuses_to_start_without_a_socket() {
        let tmp = Tmp::new("corrupt");
        let path = tmp.state_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        let fake = FakeSystemdCtl::new();
        let (_, rx) = watch::channel(false);
        let exit = run(&fake, spawn_cfg(&tmp, 2), rx).await;
        assert!(
            matches!(exit, Exit::Aborted { ref reason } if reason.contains("corrupt")),
            "{exit:?}"
        );
        assert!(!tmp.socket_path().exists(), "no socket on a refused start");
    }

    // -- protocol handling ---------------------------------------------------

    #[tokio::test]
    async fn wrong_version_and_malformed_lines_are_protocol_errors() {
        let tmp = Tmp::new("protocol");
        let (exit, resp) = drive(
            FakeSystemdCtl::new(),
            spawn_cfg(&tmp, 2),
            r#"{"v":2,"id":1,"cmd":"status"}"#,
        )
        .await;
        assert!(matches!(exit, Exit::Clean));
        assert!(!resp.ok);
        assert_eq!(resp.error, Some(ErrorCode::Protocol));
        assert!(resp.message.unwrap().contains("protocol"));

        let tmp = Tmp::new("protocol-malformed");
        let (exit, resp) = drive(
            FakeSystemdCtl::new(),
            spawn_cfg(&tmp, 2),
            "this is not json",
        )
        .await;
        assert!(matches!(exit, Exit::Clean));
        assert!(!resp.ok);
        assert_eq!(resp.error, Some(ErrorCode::Protocol));

        // A shape mismatch (status with a service) is a protocol error
        // too — with the request's id.
        let tmp = Tmp::new("protocol-shape");
        let (exit, resp) = drive(
            FakeSystemdCtl::new(),
            spawn_cfg(&tmp, 2),
            r#"{"v":1,"id":7,"cmd":"status","group":"vpn","service":"x.service"}"#,
        )
        .await;
        assert!(matches!(exit, Exit::Clean));
        assert!(!resp.ok);
        assert_eq!(resp.error, Some(ErrorCode::Protocol));
        assert_eq!(resp.id, 7);
    }

    // -- command dispatch ------------------------------------------------------

    #[tokio::test]
    async fn add_over_the_socket_persists_the_member() {
        let tmp = Tmp::new("add");
        let fake = FakeSystemdCtl::new();
        fake.set_state(
            "openvpn.service",
            UnitState {
                load_state: LoadState::Loaded,
                active_state: ActiveState::Inactive,
            },
        );

        let (exit, resp) = drive(
            fake,
            spawn_cfg(&tmp, 2),
            r#"{"v":1,"id":1,"cmd":"add","group":"vpn","service":"openvpn.service"}"#,
        )
        .await;
        assert!(matches!(exit, Exit::Clean));
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.id, 1);

        // The state file reflects the mutation (persisted by the core).
        let on_disk = fs::read_to_string(tmp.state_path()).unwrap();
        assert!(on_disk.contains("openvpn.service"));

        // The member is visible in a follow-up status.
        let tmp = Tmp::new("add-status");
        let fake = FakeSystemdCtl::new();
        fake.set_inactive("openvpn.service");
        fs::create_dir_all(tmp.state_path().parent().unwrap()).unwrap();
        fs::write(tmp.state_path(), state_with_vpn(&["openvpn.service"])).unwrap();
        let (exit, resp) =
            drive(fake, spawn_cfg(&tmp, 2), r#"{"v":1,"id":2,"cmd":"status"}"#).await;
        assert!(matches!(exit, Exit::Clean));
        assert!(resp.ok);
        let result = resp.result.unwrap();
        assert_eq!(result.groups[0].members.len(), 1);
    }

    #[tokio::test]
    async fn masked_target_renders_the_spec_error_response() {
        let tmp = Tmp::new("masked");
        let fake = FakeSystemdCtl::new();
        fake.set_masked("openvpn.service");
        fs::create_dir_all(tmp.state_path().parent().unwrap()).unwrap();
        fs::write(tmp.state_path(), state_with_vpn(&["openvpn.service"])).unwrap();

        let (exit, resp) = drive(
            fake,
            spawn_cfg(&tmp, 2),
            r#"{"v":1,"id":1,"cmd":"start","group":"vpn","service":"openvpn.service"}"#,
        )
        .await;
        assert!(matches!(exit, Exit::Clean));
        assert!(!resp.ok);
        assert_eq!(resp.error, Some(ErrorCode::UnitMasked));
        assert_eq!(
            resp.message.as_deref(),
            Some("service openvpn.service is masked")
        );
    }

    // -- bus drop / reconnect (spec §6) ----------------------------------------

    #[tokio::test]
    async fn a_newline_less_payload_over_the_ceiling_is_a_protocol_error() {
        let tmp = Tmp::new("ceiling");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let fake = FakeSystemdCtl::new();
        let daemon = run(&fake, spawn_cfg(&tmp, 2), shutdown_rx);
        let client = async {
            for _ in 0..200 {
                if UnixStream::connect(&tmp.socket_path()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Stream the ceiling plus a lot, with no newline: the daemon
            // must stop reading at the ceiling (bounded heap) and answer
            // a protocol error — not accumulate the whole payload. The
            // write may EPIPE once the daemon has responded and closed
            // (it is no longer reading the rest): that is the ceiling
            // working, so the write error is expected and ignored.
            let mut stream = UnixStream::connect(&tmp.socket_path()).await.unwrap();
            let big = vec![b'x'; super::MAX_REQUEST_LINE + 64 * 1024];
            let _ = stream.write_all(&big).await;
            let mut buf = Vec::new();
            BufReader::new(&mut stream)
                .read_until(b'\n', &mut buf)
                .await
                .unwrap();
            let resp = parse_response(std::str::from_utf8(&buf).unwrap()).unwrap();
            assert!(!resp.ok, "over the ceiling must not parse as a request");
            assert_eq!(resp.error, Some(ErrorCode::Protocol));
            let _ = shutdown_tx.send(true);
        };
        let (exit, ()) = tokio::join!(daemon, client);
        assert!(matches!(exit, Exit::Clean), "{exit:?}");
    }

    #[tokio::test]
    async fn bus_drop_triggers_reconnect_resync_and_reenabled_pumps() {
        let tmp = Tmp::new("reconnect");
        let fake = std::sync::Arc::new(FakeSystemdCtl::new());
        fake.set_active("a.service");
        fake.set_active("b.service");
        fs::create_dir_all(tmp.state_path().parent().unwrap()).unwrap();
        fs::write(
            tmp.state_path(),
            state_with_vpn(&["a.service", "b.service"]),
        )
        .unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let daemon = run(&*fake, spawn_cfg(&tmp, 2), shutdown_rx);
        let client = async {
            // Reach the socket bind.
            for _ in 0..200 {
                if UnixStream::connect(&tmp.socket_path()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            // Phase 1: an out-of-band start edge (b active, a already
            // active) → the watchdog stops a; settle it so the reaction
            // completes.
            fake.emit_state_changed("b.service", ActiveState::Active);
            settle_stop(&fake, "a.service").await;
            assert_eq!(
                fake.jobs()
                    .iter()
                    .filter(|j| j.unit == "a.service" && j.origin == JobOrigin::Stop)
                    .count(),
                1
            );

            // Phase 2: the bus drops. ussd does NOT exit — it reconnects
            // (fake: Ok), re-subscribes, re-syncs, and re-enables the
            // pumps. (The fake's channels stay alive, so the pumps'
            // re-subscribe is a no-op here; the daemon code path is
            // identical to the real backend's, where the streams end and
            // the loop re-subscribes after the re-sync.)
            fake.simulate_bus_drop();
            // Let the reconnect sequence run (fake: immediate).
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Phase 3: a NEW edge is processed again — the property under
            // test (a pre-fix daemon would sit on a dead/never-re-enabled
            // stream and never react). a becomes active → the watchdog
            // stops b.
            fake.emit_state_changed("a.service", ActiveState::Active);
            settle_stop(&fake, "b.service").await;
            assert_eq!(
                fake.jobs()
                    .iter()
                    .filter(|j| j.unit == "b.service" && j.origin == JobOrigin::Stop)
                    .count(),
                1,
                "a post-reconnect edge still triggers a reaction"
            );

            let _ = shutdown_tx.send(true);
        };
        let (exit, ()) = tokio::join!(daemon, client);
        assert!(matches!(exit, Exit::Clean), "{exit:?}");
    }

    #[tokio::test]
    async fn manager_death_beyond_budget_exits_managerdied() {
        let tmp = Tmp::new("managerdied");
        let fake = std::sync::Arc::new(FakeSystemdCtl::new());

        // Start with a healthy connect (the daemon comes up), then
        // poison `connect` and drop the bus: after the budget
        // (2 attempts, 1 s + 2 s backoff), the daemon exits ManagerDied.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let daemon = run(&*fake, spawn_cfg(&tmp, 2), shutdown_rx);
        let client = async {
            for _ in 0..200 {
                if UnixStream::connect(&tmp.socket_path()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            fake.set_connect_result(Some(Err(CtlError::ManagerAbsent)));
            fake.simulate_bus_drop();
            // The reconnect backoff: 1 s + 2 s before the exit.
            tokio::time::sleep(Duration::from_secs(4)).await;
            let _ = shutdown_tx.send(true);
        };
        let (exit, ()) = tokio::join!(daemon, client);
        assert_eq!(exit, Exit::ManagerDied, "{exit:?}");
    }
}
