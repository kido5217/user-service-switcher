//! Error model: a `thiserror` enum mirroring the §4.4 exit-code classes.
//!
//! `Display` returns the §4.4 message template WITHOUT the `uss: ` stderr
//! prefix — the CLI prepends it when printing (§4.1). `code()` is the stable
//! protocol error code (§6) for the daemon-side variants; client-side errors
//! (usage, environment) happen before any socket traffic and carry none.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Protocol-level error codes (§6), serialized in kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Exit 2 — the named group does not exist.
    UnknownGroup,
    /// Exit 3 — `remove`/`start`/`stop` on a non-member.
    NotAMember,
    /// Exit 4 — `add` on a service already in another group.
    ServiceInOtherGroup,
    /// Exit 5 — unit not found / not loadable.
    UnitNotLoadable,
    /// Exit 5 — unit masked.
    UnitMasked,
    /// Exit 6 — the systemd operation failed.
    OpFailed,
    /// Exit 8 — protocol/version mismatch.
    Protocol,
    /// Exit 8 — unexpected internal error.
    Internal,
}

impl ErrorCode {
    /// The wire form (kebab-case string).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::UnknownGroup => "unknown-group",
            ErrorCode::NotAMember => "not-a-member",
            ErrorCode::ServiceInOtherGroup => "service-in-other-group",
            ErrorCode::UnitNotLoadable => "unit-not-loadable",
            ErrorCode::UnitMasked => "unit-masked",
            ErrorCode::OpFailed => "op-failed",
            ErrorCode::Protocol => "protocol",
            ErrorCode::Internal => "internal",
        }
    }

    /// The §4.4 exit code for a daemon-reported error (§6 carries the
    /// code, not the exit; uss maps it here).
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::UnknownGroup => 2,
            ErrorCode::NotAMember => 3,
            ErrorCode::ServiceInOtherGroup => 4,
            ErrorCode::UnitNotLoadable | ErrorCode::UnitMasked => 5,
            ErrorCode::OpFailed => 6,
            ErrorCode::Protocol | ErrorCode::Internal => 8,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which unit operation failed (the `<start|stop>` slot in §4.4 row 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpVerb {
    Start,
    Stop,
}

impl fmt::Display for OpVerb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OpVerb::Start => "start",
            OpVerb::Stop => "stop",
        })
    }
}

/// The `op-failed` family (exit 6): the three §4.4 sub-templates.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum OpError {
    /// The manager rejected the call:
    /// `failed to <start|stop> <service>: <D-Bus error name> (<message>)`.
    #[error("failed to {op} {service}: {dbus_name} ({dbus_message})")]
    Rejected {
        op: OpVerb,
        service: String,
        dbus_name: String,
        dbus_message: String,
    },
    /// The job finished unsuccessfully:
    /// `failed to <start|stop> <service>: job <result>`.
    #[error("failed to {op} {service}: job {result}")]
    JobResult {
        op: OpVerb,
        service: String,
        /// The `JobRemoved` result, e.g. `failed`, `canceled`, `timeout`.
        result: String,
    },
    /// Conflicting-job rule exhausted (§4.2):
    /// `failed to stop <service>: conflicting job`.
    #[error("failed to stop {service}: conflicting job")]
    ConflictingJob { service: String },
}

/// One variant per §4.4 message template.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Exit 1 — usage (client-side, before any socket traffic).
    #[error("invalid service name: '{input}' (use 'name' or 'name.service')")]
    InvalidServiceName { input: String },

    /// Exit 1 — usage (client-side, before any socket traffic).
    #[error("invalid group name: '{input}'")]
    InvalidGroupName { input: String },

    /// Exit 1 — a usage shape clap can't express (the verb/service
    /// pairing; names are validated with the dedicated variants).
    /// Client-side, before any socket traffic.
    #[error("{message}")]
    Usage { message: String },

    /// Exit 2 — any command naming a group that does not exist.
    #[error("no such group: {group}")]
    UnknownGroup { group: String },

    /// Exit 3 — `remove`/`start`/`stop` on a non-member.
    #[error("{service} is not in group {group}")]
    NotAMember { group: String, service: String },

    /// Exit 4 — `add` on a service already in another group.
    #[error("{service} already belongs to group {other}")]
    ServiceInOtherGroup { service: String, other: String },

    /// Exit 5 — `not-found` at `add`, or unusable at `start`.
    #[error("service {service} not found")]
    ServiceNotFound { service: String },

    /// Exit 5 — masked at `add`, or unusable at `start`.
    #[error("service {service} is masked")]
    ServiceMasked { service: String },

    /// Exit 6 — the systemd operation failed.
    #[error(transparent)]
    OpFailed(#[from] OpError),

    /// Exit 7 — no user manager on the session bus (bootstrap).
    #[error("user manager not running — log in to a session or run: loginctl enable-linger {user}")]
    UserManagerAbsent { user: String },

    /// Exit 7 — ussd binary not found next to uss nor in PATH (bootstrap).
    #[error("ussd binary not found (looked next to uss and in PATH)")]
    UssdBinaryNotFound,

    /// Exit 7 — the daemon failed to start (bootstrap).
    #[error("ussd failed to start — check: journalctl --user -u ussd.service")]
    UssdFailedToStart,

    /// Exit 7 — daemon unreachable / no response / unavailable reason.
    #[error("ussd unavailable ({reason})")]
    UssdUnavailable { reason: String },

    /// Exit 8 — protocol/version mismatch.
    #[error("daemon error: {message}")]
    Protocol { message: String },

    /// Exit 8 — unexpected internal error.
    #[error("daemon error: {message}")]
    Internal { message: String },

    /// An error reported by the daemon (§6): stable code + a message safe
    /// to print verbatim. The exit code comes from the code's §4.4 row
    /// (the wire carries no exit code); the message is printed as-is.
    #[error("{message}")]
    DaemonReported { code: ErrorCode, message: String },
}

impl Error {
    /// The §4.4 exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::InvalidServiceName { .. }
            | Error::InvalidGroupName { .. }
            | Error::Usage { .. } => 1,
            Error::UnknownGroup { .. } => 2,
            Error::NotAMember { .. } => 3,
            Error::ServiceInOtherGroup { .. } => 4,
            Error::ServiceNotFound { .. } | Error::ServiceMasked { .. } => 5,
            Error::OpFailed(_) => 6,
            Error::UserManagerAbsent { .. }
            | Error::UssdBinaryNotFound
            | Error::UssdFailedToStart
            | Error::UssdUnavailable { .. } => 7,
            Error::Protocol { .. } | Error::Internal { .. } => 8,
            Error::DaemonReported { code, .. } => code.exit_code(),
        }
    }

    /// The stable protocol error code (§6) for daemon-side errors;
    /// client-side errors (usage, environment) carry none.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::UnknownGroup { .. } => Some(ErrorCode::UnknownGroup),
            Error::NotAMember { .. } => Some(ErrorCode::NotAMember),
            Error::ServiceInOtherGroup { .. } => Some(ErrorCode::ServiceInOtherGroup),
            Error::ServiceNotFound { .. } => Some(ErrorCode::UnitNotLoadable),
            Error::ServiceMasked { .. } => Some(ErrorCode::UnitMasked),
            Error::OpFailed(_) => Some(ErrorCode::OpFailed),
            Error::Protocol { .. } => Some(ErrorCode::Protocol),
            Error::Internal { .. } => Some(ErrorCode::Internal),
            Error::DaemonReported { code, .. } => Some(*code),
            Error::InvalidServiceName { .. }
            | Error::InvalidGroupName { .. }
            | Error::Usage { .. }
            | Error::UserManagerAbsent { .. }
            | Error::UssdBinaryNotFound
            | Error::UssdFailedToStart
            | Error::UssdUnavailable { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every §4.4 row: (error, exit code, protocol code, exact message).
    fn table() -> [(Error, i32, Option<ErrorCode>, &'static str); 18] {
        [
            (
                Error::InvalidServiceName {
                    input: "foo.target".into(),
                },
                1,
                None,
                "invalid service name: 'foo.target' (use 'name' or 'name.service')",
            ),
            (
                Error::InvalidGroupName {
                    input: "a/b".into(),
                },
                1,
                None,
                "invalid group name: 'a/b'",
            ),
            (
                Error::UnknownGroup {
                    group: "vpn".into(),
                },
                2,
                Some(ErrorCode::UnknownGroup),
                "no such group: vpn",
            ),
            (
                Error::NotAMember {
                    group: "vpn".into(),
                    service: "foo.service".into(),
                },
                3,
                Some(ErrorCode::NotAMember),
                "foo.service is not in group vpn",
            ),
            (
                Error::ServiceInOtherGroup {
                    service: "foo.service".into(),
                    other: "dev".into(),
                },
                4,
                Some(ErrorCode::ServiceInOtherGroup),
                "foo.service already belongs to group dev",
            ),
            (
                Error::ServiceNotFound {
                    service: "openvpn.service".into(),
                },
                5,
                Some(ErrorCode::UnitNotLoadable),
                "service openvpn.service not found",
            ),
            (
                Error::ServiceMasked {
                    service: "openvpn.service".into(),
                },
                5,
                Some(ErrorCode::UnitMasked),
                "service openvpn.service is masked",
            ),
            (
                Error::OpFailed(OpError::Rejected {
                    op: OpVerb::Start,
                    service: "foo.service".into(),
                    dbus_name: "org.freedesktop.systemd1.NoSuchUnit".into(),
                    dbus_message: "Unit foo.service not found".into(),
                }),
                6,
                Some(ErrorCode::OpFailed),
                "failed to start foo.service: org.freedesktop.systemd1.NoSuchUnit (Unit foo.service not found)",
            ),
            (
                Error::OpFailed(OpError::JobResult {
                    op: OpVerb::Stop,
                    service: "foo.service".into(),
                    result: "failed".into(),
                }),
                6,
                Some(ErrorCode::OpFailed),
                "failed to stop foo.service: job failed",
            ),
            (
                Error::OpFailed(OpError::ConflictingJob {
                    service: "foo.service".into(),
                }),
                6,
                Some(ErrorCode::OpFailed),
                "failed to stop foo.service: conflicting job",
            ),
            (
                Error::UserManagerAbsent {
                    user: "kido".into(),
                },
                7,
                None,
                "user manager not running — log in to a session or run: loginctl enable-linger kido",
            ),
            (
                Error::UssdBinaryNotFound,
                7,
                None,
                "ussd binary not found (looked next to uss and in PATH)",
            ),
            (
                Error::UssdFailedToStart,
                7,
                None,
                "ussd failed to start — check: journalctl --user -u ussd.service",
            ),
            (
                Error::UssdUnavailable {
                    reason: "no response within 30s".into(),
                },
                7,
                None,
                "ussd unavailable (no response within 30s)",
            ),
            (
                Error::Protocol {
                    message: "unsupported version 2".into(),
                },
                8,
                Some(ErrorCode::Protocol),
                "daemon error: unsupported version 2",
            ),
            (
                Error::Internal {
                    message: "boom".into(),
                },
                8,
                Some(ErrorCode::Internal),
                "daemon error: boom",
            ),
            // A daemon-reported error (§6): the exit code comes from the
            // wire code's §4.4 row; the message is printed verbatim.
            (
                Error::DaemonReported {
                    code: ErrorCode::NotAMember,
                    message: "foo.service is not in group vpn".into(),
                },
                3,
                Some(ErrorCode::NotAMember),
                "foo.service is not in group vpn",
            ),
            // A usage shape clap can't express (client-side, exit 1).
            (
                Error::Usage {
                    message: "a service is required (usage: uss vpn add <service>)".into(),
                },
                1,
                None,
                "a service is required (usage: uss vpn add <service>)",
            ),
        ]
    }

    #[test]
    fn every_row_of_4_4_maps_to_its_exit_code_code_and_message() {
        for (err, exit, code, message) in table() {
            assert_eq!(err.exit_code(), exit, "exit code for {err:?}");
            assert_eq!(err.code(), code, "protocol code for {err:?}");
            assert_eq!(&err.to_string(), message, "message for {err:?}");
        }
    }

    #[test]
    fn wire_codes_map_to_their_4_4_exit_rows() {
        for (code, exit) in [
            (ErrorCode::UnknownGroup, 2),
            (ErrorCode::NotAMember, 3),
            (ErrorCode::ServiceInOtherGroup, 4),
            (ErrorCode::UnitNotLoadable, 5),
            (ErrorCode::UnitMasked, 5),
            (ErrorCode::OpFailed, 6),
            (ErrorCode::Protocol, 8),
            (ErrorCode::Internal, 8),
        ] {
            assert_eq!(code.exit_code(), exit, "{code:?}");
        }
    }

    #[test]
    fn error_codes_serialize_in_kebab_case() {
        for (variant, wire) in [
            (ErrorCode::UnknownGroup, "unknown-group"),
            (ErrorCode::NotAMember, "not-a-member"),
            (ErrorCode::ServiceInOtherGroup, "service-in-other-group"),
            (ErrorCode::UnitNotLoadable, "unit-not-loadable"),
            (ErrorCode::UnitMasked, "unit-masked"),
            (ErrorCode::OpFailed, "op-failed"),
            (ErrorCode::Protocol, "protocol"),
            (ErrorCode::Internal, "internal"),
        ] {
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(variant.as_str(), wire);
            let back: ErrorCode = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn op_error_converts_into_error() {
        let op = OpError::JobResult {
            op: OpVerb::Start,
            service: "x.service".into(),
            result: "timeout".into(),
        };
        let err: Error = op.clone().into();
        assert_eq!(err, Error::OpFailed(op));
    }
}
