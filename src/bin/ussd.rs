//! `ussd` — the user-scope daemon (spec §6/§9).
//!
//! Thin wrapper: the runtime, socket, and lifecycle logic live in
//! `user_service_switcher::daemon`; this binary supplies the real D-Bus
//! backend and the SIGTERM/SIGINT wiring.
//!
//! Exit codes: clean stop (SIGTERM/SIGINT) → 0 (a clean stop is not a
//! failure — the unit's `Restart=on-failure` restarts real failures
//! only); manager death beyond the reconnect budget, or a refused start
//! (corrupt `groups.json`, another instance, unavailable manager) → 1.

use std::path::PathBuf;

use tokio::sync::watch;
use user_service_switcher::daemon::{self, Config};
use user_service_switcher::zbus_backend::ZbusCtl;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let exit = runtime.block_on(async {
        // Spec §6: the socket lives under `$XDG_RUNTIME_DIR` (always set
        // for a user unit; refuse to start without it).
        let Some(xdg_runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            eprintln!("ussd: $XDG_RUNTIME_DIR is not set — cannot create the socket — exiting");
            return daemon::Exit::Aborted {
                reason: "no $XDG_RUNTIME_DIR".into(),
            };
        };
        let xdg_runtime = PathBuf::from(xdg_runtime);
        // Spec §5: the state lives under `$XDG_CONFIG_HOME` (default
        // `~/.config`).
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .expect("$HOME is set for a user unit");
        let cfg = Config {
            state_path: xdg_config.join("uss").join("groups.json"),
            socket_path: xdg_runtime.join("uss").join("ussd.sock"),
            absent_budget: daemon::DEFAULT_ABSENT_BUDGET,
        };

        // SIGTERM (systemd stop / logout) and SIGINT (manual) → clean
        // shutdown.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let signal_task = tokio::spawn(async move {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("SIGINT handler");
            tokio::select! {
                _ = terminate.recv() => {},
                _ = interrupt.recv() => {},
            }
            let _ = shutdown_tx.send(true);
        });

        let ctl = ZbusCtl::new();
        let exit = daemon::run(&ctl, cfg, shutdown_rx).await;
        let _ = signal_task.await;
        exit
    });
    match exit {
        daemon::Exit::Clean => {}
        daemon::Exit::ManagerDied | daemon::Exit::Aborted { .. } => std::process::exit(1),
    }
}
