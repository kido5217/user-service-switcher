//! The `uss` CLI control plane (spec §4/§8): argument parsing, the
//! daemon-ensure bootstrap, the one-shot socket client, and the §4.3
//! status rendering.
//!
//! One-shot by construction: one socket connection per invocation, one
//! request, one response, exit (spec §4.1). Concurrent invocations are
//! serialized by ussd (the global command lock, spec §6) — the client
//! adds no locking.
//!
//! All bootstrap steps are D-Bus on the user session bus (ADR-0002) —
//! `systemctl` is never shelled. Everything the client needs from the
//! environment is injected through [`ClientPaths`], which keeps the
//! logic testable against the fake seam; the timeout is a parameter so
//! the 30 s response-wait path is unit-testable (spec §4.1).

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::error::{Error, ErrorCode};
use crate::protocol::{self, Cmd, Request, StatusResult};
use crate::systemdctl::{ActiveState, CtlError, SystemdCtl};

/// The daemon's response wait (spec §4.1): a `start` waits for its job,
/// and a slow service's start job can take a while.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The socket-readiness window (spec §8 step 5): ussd creates the socket
/// during startup; uss retries up to this long.
pub const SOCKET_RETRY: Duration = Duration::from_secs(2);

/// The ussd unit name (bootstrap, spec §8).
const USSD_UNIT: &str = "ussd.service";

/// A parsed invocation (spec §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `uss` — status of all groups.
    StatusAll,
    /// `uss <group>` — status of one group.
    StatusGroup(String),
    /// `uss <group> add <service>`.
    Add { group: String, service: String },
    /// `uss <group> remove <service>`.
    Remove { group: String, service: String },
    /// `uss <group> start <service>`.
    Start { group: String, service: String },
    /// `uss <group> stop <service>`.
    Stop { group: String, service: String },
}

/// The CLI (clap derive, spec §4.1). Plain positionals — NOT subcommands
/// — so a group named after a verb (`uss start` = status of a group
/// called `start`, §10 allows any word) does not collide with the verb
/// position; the verb/service pairing is validated in [`to_command`].
#[derive(Debug, Parser, PartialEq, Eq)]
#[command(
    name = "uss",
    version,
    about = "switch between mutually exclusive user services"
)]
pub struct Cli {
    /// Group name (omit for the status of all groups).
    group: Option<String>,

    /// A member operation.
    #[arg(value_enum)]
    verb: Option<Verb>,

    /// Service name for a member operation (`foo` or `foo.service`).
    service: Option<String>,
}

/// The member-operation verbs (spec §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Verb {
    Add,
    Remove,
    Start,
    Stop,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Add => "add",
            Verb::Remove => "remove",
            Verb::Start => "start",
            Verb::Stop => "stop",
        }
    }
}

/// Lower a parsed [`Cli`] into a [`Command`] and validate the names
/// client-side (spec §4.4 row 1: syntax errors exit 1 BEFORE any socket
/// traffic — the daemon would reject them too, but its error class is
/// exit 8; the client-side check gives the right row and skips the
/// bootstrap + socket round-trip). Usage errors are clap's own for bad
/// verb values/extra positionals; the verb/service pairing and the name
/// syntax are checked here (exit 1).
pub fn to_command(cli: Cli) -> Result<Command, Error> {
    match (cli.group, cli.verb, cli.service) {
        (None, None, None) => Ok(Command::StatusAll),
        (Some(group), None, None) => {
            crate::names::validate_group(&group)?;
            Ok(Command::StatusGroup(group))
        }
        (Some(group), Some(verb), Some(service)) => {
            crate::names::validate_group(&group)?;
            // Normalized for the wire; the daemon re-validates (it is
            // authoritative for the request).
            let service = crate::names::normalize_service(&service)?;
            Ok(match verb {
                Verb::Add => Command::Add { group, service },
                Verb::Remove => Command::Remove { group, service },
                Verb::Start => Command::Start { group, service },
                Verb::Stop => Command::Stop { group, service },
            })
        }
        (Some(group), Some(verb), None) => Err(Error::Usage {
            message: format!(
                "a service is required (usage: uss {group} {} <service>)",
                verb.as_str()
            ),
        }),
        // The remaining shapes are unreachable via positional order (the
        // group always fills first, the verb position is validated by
        // clap) — keep the match total with a generic usage error.
        _ => Err(Error::Usage {
            message: "usage: uss [group] [add|remove|start|stop] <service>".to_owned(),
        }),
    }
}

/// clap's help/version renders are success-flavored — exit 0, stdout —
/// not usage errors (spec §4.1: `--help`/`--version` standard; the
/// binary's exit-1 remap must not catch them).
pub fn is_help_or_version(err: &clap::Error) -> bool {
    matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    )
}

/// The environment the client needs, injected for testability (the binary
/// computes these from `$XDG_RUNTIME_DIR`, `$PATH`, and argv[0]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPaths {
    /// The running `uss` executable (argv[0]) — for the sibling search.
    pub uss_argv0: PathBuf,
    /// The `$PATH` value for the ussd search (None = skip it).
    pub path_var: Option<String>,
    /// `$XDG_RUNTIME_DIR/uss/ussd.sock` (spec §6).
    pub socket: PathBuf,
    /// `~/.config/systemd/user/ussd.service` (spec §9).
    pub unit_file: PathBuf,
}

/// The §9 unit file content with the (absolute) ussd binary path
/// substituted into `ExecStart` (spec §8 step 3's byte-compare target).
pub fn unit_file_content(ussd_bin: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=uss — user service switcher daemon\n\
         Documentation=https://github.com/kido5217/user-service-switcher/blob/main/docs/spec/uss-ussd.md\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=1s\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        ussd_bin.display()
    )
}

/// Locate the ussd binary: sibling of the running `uss`, else `$PATH`
/// (spec §8 step 2). Returns the ABSOLUTE path (§9: the unit's
/// `ExecStart` is absolute at write time). Pure (testable without the
/// real environment).
pub fn locate_ussd(uss_argv0: &Path, path_var: Option<&str>) -> Option<PathBuf> {
    // Sibling of the running uss (argv[0] may be relative — canonicalize
    // to the absolute form the unit file needs).
    if let Some(dir) = uss_argv0.parent().filter(|p| !p.as_os_str().is_empty()) {
        let sibling = dir.join("ussd");
        if let Ok(abs) = std::fs::canonicalize(&sibling) {
            return Some(abs);
        }
    }
    // $PATH.
    for dir in path_var.into_iter().flat_map(|p| std::env::split_paths(p)) {
        let candidate = dir.join("ussd");
        if let Ok(abs) = std::fs::canonicalize(&candidate) {
            return Some(abs);
        }
    }
    None
}

/// The `$USER` for the linger message (spec §8 step 1).
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "user".to_owned())
}

/// A seam error during bootstrap: absent manager → the linger message
/// (exit 7); anything else is unexpected (exit 8).
fn map_ctl(e: CtlError) -> Error {
    match e {
        CtlError::ManagerAbsent => Error::UserManagerAbsent {
            user: current_user(),
        },
        other => Error::Internal {
            message: other.to_string(),
        },
    }
}

/// The daemon-ensure flow (spec §8): idempotent, all D-Bus, runs at the
/// start of every command. Steps 1–4; step 5 (the socket) happens inside
/// [`execute`] — its first successful connection IS the command's
/// connection (one connection per invocation, spec §4.1).
pub async fn bootstrap<C: SystemdCtl>(ctl: &C, paths: &ClientPaths) -> Result<(), Error> {
    // 1. User manager present? (`connect` verifies the name on the bus;
    //    a missing `$XDG_RUNTIME_DIR` already failed the bus connect.)
    if let Err(e) = ctl.connect().await {
        return Err(map_ctl(e));
    }

    // 2. Locate the ussd binary (before any step that depends on the
    //    path — the unit content embeds it).
    let ussd_bin = match locate_ussd(&paths.uss_argv0, paths.path_var.as_deref()) {
        Some(p) => p,
        None => return Err(Error::UssdBinaryNotFound),
    };

    // 3. Unit installed and current? `GetUnitFileState` + byte-compare
    //    (covers a moved binary and spec revisions without needless
    //    reloads). Current states: `enabled` (what `EnableUnitFiles`
    //    produces), `static` (unreachable for this content — the written
    //    file has an `[Install]` section), `enabled-runtime` (host-
    //    grounded dash ordering; a `systemctl enable --runtime` unit is
    //    current on byte equality, else rewritten to the persistent
    //    content). Everything else (and the not-installed rejection)
    //    installs.
    let content = unit_file_content(&ussd_bin);
    let need_install = match ctl.get_unit_file_state(USSD_UNIT).await {
        Ok(state) => match state.as_str() {
            "enabled" | "static" | "enabled-runtime" => {
                match std::fs::read_to_string(&paths.unit_file) {
                    Ok(existing) => existing != content,
                    // systemd says installed, the file is missing: rewrite.
                    Err(_) => true,
                }
            }
            _ => true, // not-found / disabled / masked / generated / …
        },
        // Not installed yet: the manager rejects the query (host-
        // grounded name; the fake mirrors it).
        Err(CtlError::Rejected { ref name, .. })
            if name == "org.freedesktop.DBus.Error.FileNotFound" =>
        {
            true
        }
        Err(other) => return Err(map_ctl(other)),
    };
    if need_install {
        if let Some(dir) = paths
            .unit_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(dir).map_err(|e| Error::Internal {
                message: format!("cannot create {}: {e}", dir.display()),
            })?;
        }
        std::fs::write(&paths.unit_file, &content).map_err(|e| Error::Internal {
            message: format!("cannot write {}: {e}", paths.unit_file.display()),
        })?;
        ctl.reload().await.map_err(map_ctl)?;
        let units = vec![USSD_UNIT.to_owned()];
        ctl.enable_unit_files(&units).await.map_err(map_ctl)?;
    }

    // 4. Daemon running? If not: start it, wait for its `JobRemoved`
    //    (done/skipped), verify active. A rejected start (bad unit
    //    setting, no dependencies) is the §8 step-4 failure: exit 7 with
    //    the journal pointer — the journal names the cause.
    let state = ctl.get_unit_state(USSD_UNIT).await.map_err(map_ctl)?;
    if state.active_state != ActiveState::Active {
        // Subscribe BEFORE the start (the fake's streams are no-replay;
        // a job that settled before the subscribe would be lost).
        let mut rx = ctl.job_removed();
        let job = match ctl.start_unit(USSD_UNIT).await {
            Ok(job) => job,
            Err(_) => return Err(Error::UssdFailedToStart),
        };
        // Wait for this job's removal (different units' jobs pass by).
        let removed: Option<_> = loop {
            match rx.recv().await {
                Ok(r) if r.id == job.id => break Some(r),
                Ok(_other) => continue,
                Err(_) => break None, // the stream ended: the manager went away
            }
        };
        let Some(removed) = removed else {
            return Err(Error::UssdFailedToStart);
        };
        if !removed.result.is_success() {
            return Err(Error::UssdFailedToStart);
        }
        let state = ctl.get_unit_state(USSD_UNIT).await.map_err(map_ctl)?;
        if state.active_state != ActiveState::Active {
            return Err(Error::UssdFailedToStart);
        }
    }
    Ok(())
}

/// Run one invocation: bootstrap (spec §8) → one connection (step 5:
/// retry up to 2 s) → one request → one response (`response_timeout`,
/// 30 s per spec §4.1 — the operation continues inside ussd and is
/// visible via status on timeout).
///
/// Returns the status payload (`Some` for `status` commands; `None` for
/// mutations — they print nothing on success, §4.1). Client-side errors
/// (environment, timeout) and daemon-reported errors (§6 codes) both
/// surface as [`Error`]s; the binary prints `uss: <message>` and exits
/// with the §4.4 code.
pub async fn execute<C: SystemdCtl>(
    ctl: &C,
    paths: &ClientPaths,
    cmd: Command,
    response_timeout: Duration,
) -> Result<Option<StatusResult>, Error> {
    bootstrap(ctl, paths).await?;

    let request = match &cmd {
        Command::StatusAll => Request::status_all(1),
        Command::StatusGroup(group) => Request::status_group(1, group),
        Command::Add { group, service } => Request::mutate(1, Cmd::Add, group, service),
        Command::Remove { group, service } => Request::mutate(1, Cmd::Remove, group, service),
        Command::Start { group, service } => Request::mutate(1, Cmd::Start, group, service),
        Command::Stop { group, service } => Request::mutate(1, Cmd::Stop, group, service),
    };

    // Step 5: socket ready — retry up to 2 s (ussd creates it during
    // startup). The first successful connection is the command's
    // connection.
    let mut stream = connect_with_retry(&paths.socket, SOCKET_RETRY)
        .await
        .map_err(|_| Error::UssdUnavailable {
            reason: "socket not ready".into(),
        })?;

    stream
        .write_all(protocol::encode(&request).as_bytes())
        .await
        .map_err(|e| Error::UssdUnavailable {
            reason: format!("write failed: {e}"),
        })?;

    // Exactly one response (the daemon closes after it); wait at most
    // `response_timeout` for it.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<(), std::io::Error>(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') {
                return Ok(());
            }
        }
    };
    match tokio::time::timeout(response_timeout, read).await {
        Err(_) => {
            return Err(Error::UssdUnavailable {
                reason: format!("no response within {response_timeout:?}"),
            });
        }
        Ok(Err(e)) => {
            return Err(Error::UssdUnavailable {
                reason: format!("no response ({e})"),
            });
        }
        Ok(Ok(())) => {}
    }
    let line_end = buf.iter().position(|b| *b == b'\n').unwrap_or(buf.len());
    let line = String::from_utf8_lossy(&buf[..line_end]);
    let response = protocol::parse_response(&line).map_err(|e| Error::Protocol {
        message: e.to_string(),
    })?;
    if response.ok {
        // `result` is present for status, absent for mutations.
        Ok(response.result)
    } else {
        // The daemon's stable code + the §4.4 message verbatim (spec §6):
        // the exit code comes from the code, the message is printed as-is.
        Err(Error::DaemonReported {
            code: response.error.unwrap_or(ErrorCode::Internal),
            message: response
                .message
                .unwrap_or_else(|| "daemon error: (no message)".to_owned()),
        })
    }
}

/// Connect with retries up to `budget` (spec §8 step 5).
async fn connect_with_retry(socket: &Path, budget: Duration) -> Result<UnixStream, std::io::Error> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match UnixStream::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Render a status response per §4.3: groups alphabetical (the daemon
/// orders them), members indented two spaces in add order, the running
/// member suffixed ` - Active`, transient states bare. No groups → no
/// output.
pub fn render_status(result: &StatusResult) -> String {
    let mut out = String::new();
    for group in &result.groups {
        out.push_str(&group.name);
        out.push('\n');
        for member in &group.members {
            out.push_str("  ");
            out.push_str(&member.name);
            if member.active {
                out.push_str(" - Active");
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MemberStatus, Response};
    use crate::systemdctl::{CtlError, FakeSystemdCtl, JobOrigin, JobResult};
    use std::fs;
    use tokio::net::UnixListener;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("uss-client-test-{}-{tag}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn paths(&self) -> ClientPaths {
            ClientPaths {
                uss_argv0: self.0.join("bin").join("uss"),
                path_var: None,
                socket: self.0.join("runtime").join("uss").join("ussd.sock"),
                unit_file: self
                    .0
                    .join("config")
                    .join("systemd")
                    .join("user")
                    .join("ussd.service"),
            }
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The running uss + sibling ussd in place (bootstrap step 2).
    fn with_binaries(tmp: &Tmp) {
        let bin = tmp.0.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("uss"), b"").unwrap();
        fs::write(bin.join("ussd"), b"").unwrap();
    }

    /// A current unit file on disk (bootstrap step 3's byte-compare
    /// passes).
    fn with_current_unit_file(tmp: &Tmp, paths: &ClientPaths) {
        let bin = tmp.0.join("bin");
        fs::create_dir_all(paths.unit_file.parent().unwrap()).unwrap();
        fs::write(
            paths.unit_file.clone(),
            unit_file_content(&bin.join("ussd")),
        )
        .unwrap();
    }

    // -- parsing (spec §4.1/§4.2) ------------------------------------------

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn parsing_covers_the_command_matrix() {
        assert_eq!(to_command(cli(&["uss"])).unwrap(), Command::StatusAll);
        assert_eq!(
            to_command(cli(&["uss", "vpn"])).unwrap(),
            Command::StatusGroup("vpn".into())
        );
        assert_eq!(
            to_command(cli(&["uss", "vpn", "add", "foo"])).unwrap(),
            // The service is normalized client-side (§10): the bare name
            // rides the wire as `foo.service`.
            Command::Add {
                group: "vpn".into(),
                service: "foo.service".into()
            }
        );
        for (verb, cmd) in [
            (
                "remove",
                Command::Remove {
                    group: "vpn".into(),
                    service: "foo.service".into(),
                },
            ),
            (
                "start",
                Command::Start {
                    group: "vpn".into(),
                    service: "foo.service".into(),
                },
            ),
            (
                "stop",
                Command::Stop {
                    group: "vpn".into(),
                    service: "foo.service".into(),
                },
            ),
        ] {
            assert_eq!(
                to_command(cli(&["uss", "vpn", verb, "foo.service"])).unwrap(),
                cmd,
                "{verb}"
            );
        }
    }

    #[test]
    fn usage_errors_are_clap_rejections() {
        // An invalid verb value / extra positionals → clap's own messages
        // (exit 1 at the binary).
        for args in [
            vec!["uss", "vpn", "fly", "foo"],
            vec!["uss", "vpn", "start", "foo", "bar"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        // A verb without a service parses (the positionals fill), but the
        // pairing check rejects it (exit 1, local message).
        let err = to_command(cli(&["uss", "vpn", "add"])).unwrap_err();
        assert!(err.to_string().contains("a service is required"), "{err}");
        assert_eq!(err.exit_code(), 1);
        // A group named after a verb is a plain group (status).
        assert_eq!(
            to_command(cli(&["uss", "start"])).unwrap(),
            Command::StatusGroup("start".into())
        );
    }

    #[test]
    fn invalid_names_are_client_side_usage_errors() {
        // Spec §4.4 row 1 + §10: exit 1, the §4.4 template message,
        // before any socket traffic.
        let err = to_command(cli(&["uss", "vpn", "add", "foo.target"])).unwrap_err();
        assert!(matches!(err, Error::InvalidServiceName { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            err.to_string(),
            "invalid service name: 'foo.target' (use 'name' or 'name.service')"
        );
        let err = to_command(cli(&["uss", "bad group"])).unwrap_err();
        assert!(matches!(err, Error::InvalidGroupName { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 1);
        // A verb command with a bad group name is a row-1 error too (not
        // a socket round-trip that would surface as exit 8).
        let err = to_command(cli(&["uss", "bad group", "add", "foo.service"])).unwrap_err();
        assert!(matches!(err, Error::InvalidGroupName { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn help_and_version_are_success_flavored() {
        // Spec §4.1: `--help`/`--version` standard — the binary prints
        // them to stdout and exits 0 (is_help_or_version is that check).
        let err = Cli::try_parse_from(["uss", "--help"]).unwrap_err();
        assert!(
            is_help_or_version(&err),
            "--help is a success-flavored kind"
        );
        let err = Cli::try_parse_from(["uss", "--version"]).unwrap_err();
        assert!(
            is_help_or_version(&err),
            "--version is a success-flavored kind"
        );
        // A genuine usage error is NOT success-flavored (exit 1).
        let err = Cli::try_parse_from(["uss", "vpn", "fly", "foo"]).unwrap_err();
        assert!(!is_help_or_version(&err));
    }

    // -- locating + unit content (spec §8 step 2/3) -------------------------

    #[test]
    fn locate_ussd_prefers_the_sibling_then_path() {
        let tmp = Tmp::new("locate");
        let bin = tmp.0.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let sibling = bin.join("ussd");
        fs::write(&sibling, b"").unwrap();
        let pathbin = tmp.0.join("pathbin");
        fs::create_dir_all(&pathbin).unwrap();
        let on_path = pathbin.join("ussd");
        fs::write(&on_path, b"").unwrap();

        assert_eq!(locate_ussd(&bin.join("uss"), None), Some(sibling.clone()));
        // The sibling wins over the PATH entry.
        assert_eq!(
            locate_ussd(&bin.join("uss"), Some(pathbin.to_str().unwrap())),
            Some(sibling)
        );
        // No sibling → the PATH entry.
        assert_eq!(
            locate_ussd(
                &tmp.0.join("elsewhere").join("uss"),
                Some(pathbin.to_str().unwrap())
            ),
            Some(on_path)
        );
        // Neither → None.
        assert_eq!(
            locate_ussd(
                &tmp.0.join("nowhere").join("uss"),
                Some(tmp.0.join("empty").to_str().unwrap())
            ),
            None
        );
    }

    #[test]
    fn unit_file_content_matches_the_spec_nine_shape() {
        let content = unit_file_content(Path::new("/home/u/.local/bin/ussd"));
        assert!(content.starts_with("[Unit]\n"));
        assert!(content.contains("ExecStart=/home/u/.local/bin/ussd\n"));
        assert!(content.contains("Restart=on-failure\n"));
        assert!(content.contains("RestartSec=1s\n"));
        assert!(content.contains("WantedBy=default.target\n"));
        assert!(!content.contains("socket"), "no socket-activation unit");
    }

    // -- bootstrap (spec §8, fake side) --------------------------------------

    /// A fake + paths with the ussd sibling in place.
    fn bootstrap_fixture(tag: &str) -> (FakeSystemdCtl, Tmp, ClientPaths) {
        let tmp = Tmp::new(tag);
        with_binaries(&tmp);
        let paths = tmp.paths();
        (FakeSystemdCtl::new(), tmp, paths)
    }

    #[tokio::test]
    async fn bootstrap_missing_manager_is_exit_7_linger() {
        let (fake, _tmp, paths) = bootstrap_fixture("no-manager");
        fake.set_connect_result(Some(Err(CtlError::ManagerAbsent)));
        let err = bootstrap(&fake, &paths).await.unwrap_err();
        assert!(matches!(err, Error::UserManagerAbsent { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 7);
    }

    #[tokio::test]
    async fn bootstrap_missing_binary_is_exit_7() {
        let (fake, tmp, _) = bootstrap_fixture("no-binary");
        let paths = ClientPaths {
            uss_argv0: tmp.0.join("nowhere").join("uss"),
            ..tmp.paths()
        };
        let err = bootstrap(&fake, &paths).await.unwrap_err();
        assert!(matches!(err, Error::UssdBinaryNotFound), "{err:?}");
        assert_eq!(err.exit_code(), 7);
    }

    #[tokio::test]
    async fn bootstrap_installs_starts_and_is_idempotent() {
        let (fake, tmp, paths) = bootstrap_fixture("install");
        // Run 1: the unit is not installed (the fake rejects the query)
        // and the daemon is not active → install + start.
        fake.set_inactive(USSD_UNIT);
        let (result, ()) = tokio::join!(bootstrap(&fake, &paths), async {
            // Settle the daemon's start job (the fake never settles
            // on its own): the state is set BEFORE the settle signal,
            // so the bootstrap's post-settle state read is
            // deterministic.
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(id) = fake.pending_job(USSD_UNIT) {
                    fake.set_active(USSD_UNIT);
                    fake.settle_job(id, JobResult::Done);
                    return;
                }
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        result.unwrap();

        // The unit file was written with the spec content…
        let written = fs::read_to_string(paths.unit_file.clone()).unwrap();
        assert!(written.contains(&format!(
            "ExecStart={}\n",
            tmp.0.join("bin").join("ussd").display()
        )));
        // …and the install sequence ran: Reload + EnableUnitFiles.
        assert_eq!(fake.reloads(), 1);
        assert_eq!(fake.enable_calls(), vec![vec![USSD_UNIT.to_string()]]);
        // The daemon was started exactly once.
        assert_eq!(
            fake.jobs()
                .iter()
                .filter(|j| j.unit == USSD_UNIT && j.origin == JobOrigin::Start)
                .count(),
            1
        );

        // Run 2 (the "current" state: enabled + byte-identical +
        // active): no write, no reload, no enable, no start.
        fake.set_unit_file_state(USSD_UNIT, "enabled");
        bootstrap(&fake, &paths).await.unwrap();
        assert_eq!(fake.reloads(), 1, "the second run is a no-op (no reload)");
        assert_eq!(
            fake.enable_calls().len(),
            1,
            "the second run is a no-op (no enable)"
        );
        assert_eq!(
            fake.jobs().iter().filter(|j| j.unit == USSD_UNIT).count(),
            1,
            "the second run does not start the daemon"
        );
        // The unit file is untouched by the second run (the byte-compare
        // passed).
        assert_eq!(fs::read_to_string(paths.unit_file).unwrap(), written);
    }

    #[tokio::test]
    async fn bootstrap_reinstalls_a_moved_binary() {
        let (fake, tmp, paths) = bootstrap_fixture("moved");
        // An up-to-date install + an active daemon: the bootstrap is a
        // no-op.
        fake.set_active(USSD_UNIT);
        fake.set_unit_file_state(USSD_UNIT, "enabled");
        with_current_unit_file(&tmp, &paths);
        bootstrap(&fake, &paths).await.unwrap();
        assert_eq!(fake.reloads(), 0, "an up-to-date unit needs no reload");

        // The binary moved: the sibling is gone, PATH provides the new
        // home. The byte-compare now differs → reinstall.
        let moved_dir = tmp.0.join("moved-bin");
        fs::create_dir_all(&moved_dir).unwrap();
        fs::write(moved_dir.join("ussd"), b"").unwrap();
        fs::remove_file(tmp.0.join("bin").join("ussd")).unwrap();
        let moved_paths = ClientPaths {
            path_var: Some(moved_dir.to_str().unwrap().to_owned()),
            ..paths.clone()
        };
        bootstrap(&fake, &moved_paths).await.unwrap();
        assert_eq!(fake.reloads(), 1, "a moved binary rewrites + reloads");
        let written = fs::read_to_string(paths.unit_file).unwrap();
        assert!(written.contains(&format!("ExecStart={}\n", moved_dir.join("ussd").display())));
    }

    #[tokio::test]
    async fn bootstrap_a_runtime_enabled_unit_is_current_on_byte_equality() {
        // Host-grounded: real systemd reports `enabled-runtime` (dash
        // ordering) for a `systemctl enable --runtime` unit — it is in
        // the current set, so a byte-identical file needs no reinstall.
        let (fake, tmp, paths) = bootstrap_fixture("runtime-enabled");
        fake.set_active(USSD_UNIT);
        fake.set_unit_file_state(USSD_UNIT, "enabled-runtime");
        with_current_unit_file(&tmp, &paths);
        bootstrap(&fake, &paths).await.unwrap();
        assert_eq!(
            fake.reloads(),
            0,
            "a runtime-enabled unit with identical bytes is current"
        );
        assert_eq!(fake.enable_calls(), Vec::<Vec<String>>::new());
    }

    #[tokio::test]
    async fn bootstrap_a_failed_daemon_start_is_exit_7() {
        let (fake, _tmp, paths) = bootstrap_fixture("startfailed");
        fake.set_inactive(USSD_UNIT);
        // The start job settles `failed` → UssdFailedToStart (exit 7).
        let (result, ()) = tokio::join!(bootstrap(&fake, &paths), async {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(id) = fake.pending_job(USSD_UNIT) {
                    fake.settle_job(id, JobResult::Failed);
                    return;
                }
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let err = result.unwrap_err();
        assert!(matches!(err, Error::UssdFailedToStart), "{err:?}");
        assert_eq!(err.exit_code(), 7);
    }

    // -- the client (socket stub) --------------------------------------------

    /// A socket stub: accepts one connection and answers `answer` (or
    /// stays silent for the timeout test).
    async fn stub_server(socket: &Path, answer: Option<String>) {
        if let Some(dir) = socket.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            if let Some(answer) = answer {
                let _ = stream.write_all(answer.as_bytes()).await;
            } else {
                // Silent: hold the connection open past the client's
                // timeout (the client gives up; the hold ends shortly
                // after the test).
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// A fake + paths with a current unit file and an active daemon —
    /// the bootstrap is a no-op, so the client tests reach the socket.
    async fn client_fixture(tag: &str) -> (FakeSystemdCtl, Tmp, ClientPaths) {
        let (fake, tmp, paths) = bootstrap_fixture(tag);
        fake.set_unit_file_state(USSD_UNIT, "enabled");
        with_current_unit_file(&tmp, &paths);
        fake.set_active(USSD_UNIT);
        (fake, tmp, paths)
    }

    #[tokio::test]
    async fn a_version_mismatch_response_is_a_protocol_error() {
        let (fake, _tmp, paths) = client_fixture("version").await;
        stub_server(
            &paths.socket,
            Some("{\"v\":2,\"id\":1,\"ok\":true}\n".into()),
        )
        .await;
        let err = execute(&fake, &paths, Command::StatusAll, RESPONSE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 8);
        assert!(err.to_string().contains("unsupported protocol version 2"));
    }

    #[tokio::test]
    async fn a_silent_daemon_times_out_as_unavailable() {
        let (fake, _tmp, paths) = client_fixture("timeout").await;
        stub_server(&paths.socket, None).await;
        let err = execute(
            &fake,
            &paths,
            Command::StatusAll,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::UssdUnavailable { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("no response within 300ms"));
    }

    #[tokio::test]
    async fn a_daemon_error_response_maps_code_and_message_verbatim() {
        let (fake, _tmp, paths) = client_fixture("wire-error").await;
        stub_server(
            &paths.socket,
            Some(
                "{\"v\":1,\"id\":1,\"ok\":false,\"error\":\"unit-masked\",\"message\":\"service openvpn.service is masked\"}\n"
                    .into(),
            ),
        )
        .await;
        let err = execute(
            &fake,
            &paths,
            Command::Start {
                group: "vpn".into(),
                service: "openvpn.service".into(),
            },
            RESPONSE_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::DaemonReported {
                    code: ErrorCode::UnitMasked,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 5);
        assert_eq!(err.to_string(), "service openvpn.service is masked");
    }

    #[tokio::test]
    async fn a_status_response_renders_per_section_4_3() {
        let (fake, _tmp, paths) = client_fixture("status-render").await;
        let status = Response::ok(
            1,
            crate::protocol::StatusResult {
                groups: vec![
                    crate::protocol::GroupStatus {
                        name: "dev".into(),
                        members: vec![MemberStatus {
                            name: "foo.service".into(),
                            active: false,
                        }],
                    },
                    crate::protocol::GroupStatus {
                        name: "vpn".into(),
                        members: vec![
                            MemberStatus {
                                name: "openvpn.service".into(),
                                active: true,
                            },
                            MemberStatus {
                                name: "wireguard.service".into(),
                                active: false,
                            },
                        ],
                    },
                ],
            },
        );
        stub_server(&paths.socket, Some(protocol::encode(&status))).await;
        let status = execute(&fake, &paths, Command::StatusAll, RESPONSE_TIMEOUT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            render_status(&status),
            "dev\n  foo.service\nvpn\n  openvpn.service - Active\n  wireguard.service\n"
        );
    }

    #[test]
    fn rendering_an_empty_status_prints_nothing() {
        assert_eq!(render_status(&StatusResult { groups: vec![] }), "");
    }
}
