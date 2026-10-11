//! `uss` — CLI control tool for groups of mutually exclusive user services.
//!
//! Thin wrapper (spec §4/§8): argument parsing, the daemon-ensure
//! bootstrap, and the one-shot socket client live in
//! `user_service_switcher::uss_client`; this binary supplies the real
//! D-Bus backend and the environment paths, prints `uss: <message>`
//! errors to stderr, and exits with the §4.4 code.
//!
//! Test hook (spec §8): a debug build with `USS_TEST_SKIP_BOOTSTRAP` set
//! in the environment skips the bootstrap (steps 1–4) and sends the
//! command straight to `$XDG_RUNTIME_DIR/uss/ussd.sock` — the offline CLI
//! harness (`tests/cli.rs`) points it at a fake ussd on a temp socket.
//! The missing-`$XDG_RUNTIME_DIR` guard above still applies when armed
//! (step 5 locates the socket from that dir). `cfg!(debug_assertions)`
//! short-circuits the read in release builds, so the shipped binary
//! ignores the variable entirely.

use std::path::PathBuf;

use clap::Parser;
use user_service_switcher::uss_client::{self, ClientPaths, RESPONSE_TIMEOUT};
use user_service_switcher::zbus_backend::ZbusCtl;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(async {
        // clap's own messages for flag/value errors — but the §4.4 usage
        // class is exit 1 (clap's default is 2), and `--help`/`--version`
        // are success-flavored (exit 0, stdout), so the parse is handled
        // here rather than by `parse()`.
        let cli = match uss_client::Cli::try_parse() {
            Ok(cli) => cli,
            Err(err) => {
                if uss_client::is_help_or_version(&err) {
                    println!("{err}");
                    return 0;
                }
                eprintln!("{err}");
                return 1;
            }
        };
        let cmd = match uss_client::to_command(cli) {
            Ok(cmd) => cmd,
            // A usage error or a name-syntax error (spec §4.4 row 1):
            // client-side, before any socket traffic — the exit code is
            // the error's row (1).
            Err(e) => {
                eprintln!("uss: {e}");
                return e.exit_code();
            }
        };

        // The environment (spec §8 step 1: a missing `$XDG_RUNTIME_DIR`
        // means no user session → the linger message, exit 7).
        let xdg_runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                eprintln!(
                    "uss: {}",
                    user_service_switcher::error::Error::UserManagerAbsent {
                        user: std::env::var("USER")
                            .or_else(|_| std::env::var("LOGNAME"))
                            .unwrap_or_else(|_| "user".to_owned())
                    }
                );
                return 7;
            }
        };
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_default();
        let paths = ClientPaths {
            uss_argv0: std::env::args()
                .next()
                .unwrap_or_else(|| "uss".to_owned())
                .into(),
            path_var: std::env::var_os("PATH").map(|p| p.to_string_lossy().into_owned()),
            socket: xdg_runtime.join("uss").join("ussd.sock"),
            unit_file: xdg_config.join("systemd").join("user").join("ussd.service"),
        };

        // The bootstrap is skipped only for the offline CLI harness
        // (spec §8/§11); release builds never honor the variable, and the
        // bus backend is not even constructed on the skipped path.
        let skip_bootstrap =
            cfg!(debug_assertions) && std::env::var_os("USS_TEST_SKIP_BOOTSTRAP").is_some();
        let result = if skip_bootstrap {
            uss_client::send(&paths, cmd, RESPONSE_TIMEOUT).await
        } else {
            let ctl = ZbusCtl::new();
            uss_client::execute(&ctl, &paths, cmd, RESPONSE_TIMEOUT).await
        };
        match result {
            Ok(Some(status)) => {
                // Only `status` prints (spec §4.1: stdout carries status
                // data only; mutations print nothing on success).
                print!("{}", uss_client::render_status(&status));
                0
            }
            Ok(None) => 0,
            Err(e) => {
                eprintln!("uss: {e}");
                e.exit_code()
            }
        }
    });
    std::process::exit(code);
}
