# ussd is a control plane and watchdog daemon

A pure CLI could have done add/remove/start/stop and status with a local state file, but exclusivity must hold continuously, not only when `uss` runs. `ussd` (a systemd user service) is the single writer: it owns the group state, issues every systemd operation, and watches for out-of-band member starts to stop the group's other active members. `uss` is a thin client over a Unix socket.

**Considered options**: control plane only (rejected — the invariant breaks silently on out-of-band starts); watchdog only, with the CLI talking to systemd directly (rejected — two writers on state and on systemd operations); state store only (rejected — the daemon would not earn its keep).

**Consequences**: every `uss` command requires a running daemon, hence the first-use auto-install bootstrap; the watchdog needs a reliable state-change detection mechanism (D-Bus signals vs polling — decided by the systemd-interface research); the tool adds a unit to the user session and a socket under the runtime dir.
