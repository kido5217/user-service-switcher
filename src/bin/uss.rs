//! `uss` — CLI control tool for groups of mutually exclusive user services.
//!
//! Thin wrapper (spec §4/§8): argument parsing, the daemon-ensure
//! bootstrap, and the one-shot socket client live in
//! `user_service_switcher::uss_client`; this binary supplies the real
//! D-Bus backend and the environment paths, prints `uss: <message>`
//! errors to stderr, and exits with the §4.4 code.

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

        let ctl = ZbusCtl::new();
        match uss_client::execute(&ctl, &paths, cmd, RESPONSE_TIMEOUT).await {
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
