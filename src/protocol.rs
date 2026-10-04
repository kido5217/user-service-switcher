//! Wire protocol between uss and ussd (spec §6).
//!
//! Unix stream socket, one JSON object per line (newline-terminated). One
//! request per connection from uss; exactly one response from ussd. Every
//! message carries `"v": 1`; other versions are rejected (exit 8 class).

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::ErrorCode;

/// The only protocol version.
pub const PROTOCOL_VERSION: u8 = 1;

/// Parse/shape error for a wire message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// The line is not a well-formed message of the expected shape.
    #[error("malformed message: {0}")]
    Malformed(String),
    /// The message carries a protocol version other than 1.
    #[error("unsupported protocol version {0} (expected {PROTOCOL_VERSION})")]
    UnsupportedVersion(u8),
}

/// Commands, serialized lowercase (§6 request examples).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cmd {
    Status,
    Add,
    Remove,
    Start,
    Stop,
}

/// A uss → ussd request (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u8,
    /// 1 for one-shot uss (kept for symmetry/future).
    pub id: u64,
    pub cmd: Cmd,
    /// Optional only for `status` (absent = all groups).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

impl Request {
    /// `{"v":1,"id":<id>,"cmd":"status"}` — status of all groups.
    pub fn status_all(id: u64) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            cmd: Cmd::Status,
            group: None,
            service: None,
        }
    }

    /// `{"v":1,"id":<id>,"cmd":"status","group":<group>}`.
    pub fn status_group(id: u64, group: &str) -> Self {
        Self {
            group: Some(group.to_owned()),
            ..Self::status_all(id)
        }
    }

    /// A mutation request: `{"v":1,"id":<id>,"cmd":<cmd>,"group":…,"service":…}`.
    pub fn mutate(id: u64, cmd: Cmd, group: &str, service: &str) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            cmd,
            group: Some(group.to_owned()),
            service: Some(service.to_owned()),
        }
    }
}

/// One member in a status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStatus {
    pub name: String,
    /// `ActiveState == "active"` read live at query time.
    pub active: bool,
}

/// One group in a status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupStatus {
    pub name: String,
    /// Members in add order.
    pub members: Vec<MemberStatus>,
}

/// The `result` payload of a `status` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    /// Groups in alphabetical order.
    pub groups: Vec<GroupStatus>,
}

/// A ussd → uss response (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub v: u8,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<StatusResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Response {
    /// A successful `status` response.
    pub fn ok(id: u64, result: StatusResult) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            result: Some(result),
            error: None,
            message: None,
        }
    }

    /// An error response: stable code + human-readable message, safe to
    /// print verbatim.
    pub fn err(id: u64, error: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(error),
            message: Some(message.into()),
        }
    }
}

/// Encode a message as one newline-terminated JSON line (§6 framing).
pub fn encode<T: Serialize>(message: &T) -> String {
    format!(
        "{}\n",
        serde_json::to_string(message).expect("protocol messages serialize infallibly")
    )
}

/// Parse one request line (trailing newline optional); rejects other
/// protocol versions (§6 versioning).
pub fn parse_request(line: &str) -> Result<Request, ProtocolError> {
    parse_versioned(line)
}

/// Parse one response line (trailing newline optional); rejects other
/// protocol versions (§6 versioning).
pub fn parse_response(line: &str) -> Result<Response, ProtocolError> {
    parse_versioned(line)
}

/// Parse a message and enforce the protocol version (§6 versioning):
/// the line must be a JSON object with `"v": 1`, else rejected.
fn parse_versioned<T: DeserializeOwned>(line: &str) -> Result<T, ProtocolError> {
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
    let version = value
        .get("v")
        .and_then(|v| v.as_u64())
        .filter(|v| *v <= u8::MAX as u64)
        .ok_or_else(|| ProtocolError::Malformed("missing or invalid `v` field".into()))?;
    if version as u8 != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version as u8));
    }
    serde_json::from_value(value).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, active: bool) -> MemberStatus {
        MemberStatus {
            name: name.into(),
            active,
        }
    }

    /// The six §6 request examples, verbatim.
    const REQUESTS: &[&str] = &[
        r#"{"v":1,"id":1,"cmd":"status"}"#,
        r#"{"v":1,"id":1,"cmd":"status","group":"vpn"}"#,
        r#"{"v":1,"id":1,"cmd":"add","group":"vpn","service":"openvpn.service"}"#,
        r#"{"v":1,"id":1,"cmd":"remove","group":"vpn","service":"openvpn.service"}"#,
        r#"{"v":1,"id":1,"cmd":"start","group":"vpn","service":"openvpn.service"}"#,
        r#"{"v":1,"id":1,"cmd":"stop","group":"vpn","service":"openvpn.service"}"#,
    ];

    #[test]
    fn spec_request_examples_round_trip_verbatim() {
        for (n, expected) in REQUESTS.iter().enumerate() {
            let parsed = parse_request(expected).unwrap();
            assert_eq!(encode(&parsed).trim(), *expected, "request {n}");
        }
    }

    #[test]
    fn constructors_build_the_spec_shapes() {
        assert_eq!(
            Request::status_all(1),
            parse_request(r#"{"v":1,"id":1,"cmd":"status"}"#).unwrap()
        );
        assert_eq!(
            Request::status_group(1, "vpn"),
            parse_request(r#"{"v":1,"id":1,"cmd":"status","group":"vpn"}"#).unwrap()
        );
        assert_eq!(
            Request::mutate(1, Cmd::Add, "vpn", "openvpn.service"),
            parse_request(
                r#"{"v":1,"id":1,"cmd":"add","group":"vpn","service":"openvpn.service"}"#
            )
            .unwrap()
        );
    }

    #[test]
    fn ok_response_matches_the_spec_example_verbatim() {
        let response = Response::ok(
            1,
            StatusResult {
                groups: vec![GroupStatus {
                    name: "vpn".into(),
                    members: vec![
                        member("openvpn.service", true),
                        member("wireguard.service", false),
                    ],
                }],
            },
        );
        assert_eq!(
            encode(&response).trim(),
            r#"{"v":1,"id":1,"ok":true,"result":{"groups":[{"name":"vpn","members":[{"name":"openvpn.service","active":true},{"name":"wireguard.service","active":false}]}]}}"#
        );
        let parsed = parse_response(&encode(&response)).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.result.unwrap().groups.len(), 1);
    }

    #[test]
    fn error_response_matches_the_spec_example_verbatim() {
        let response = Response::err(
            1,
            ErrorCode::UnitNotLoadable,
            "service openvpn.service not found",
        );
        assert_eq!(
            encode(&response).trim(),
            r#"{"v":1,"id":1,"ok":false,"error":"unit-not-loadable","message":"service openvpn.service not found"}"#
        );
        let parsed = parse_response(&encode(&response)).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error, Some(ErrorCode::UnitNotLoadable));
    }

    #[test]
    fn framing_is_one_json_object_per_line() {
        let line = encode(&Request::status_all(1));
        assert!(line.ends_with('\n'));
        // Parsing tolerates the trailing newline and surrounding whitespace.
        assert_eq!(parse_request(&line).unwrap().cmd, Cmd::Status);
        assert_eq!(
            parse_request("   \n").unwrap_err(),
            ProtocolError::Malformed(serde_json::from_str::<Request>("").unwrap_err().to_string())
        );
    }

    #[test]
    fn other_versions_are_rejected() {
        assert_eq!(
            parse_request(r#"{"v":2,"id":1,"cmd":"status"}"#).unwrap_err(),
            ProtocolError::UnsupportedVersion(2)
        );
        assert_eq!(
            parse_response(r#"{"v":0,"id":1,"ok":true}"#).unwrap_err(),
            ProtocolError::UnsupportedVersion(0)
        );
    }

    #[test]
    fn malformed_lines_are_rejected() {
        for line in [
            "not json",
            r#"{"v":1,"id":1,"cmd":"fly"}"#,
            r#"{"v":1,"id":1,"cmd":"status","group":42}"#,
        ] {
            assert!(
                matches!(parse_request(line), Err(ProtocolError::Malformed(_))),
                "{line:?} must be malformed"
            );
            assert!(
                matches!(parse_response(line), Err(ProtocolError::Malformed(_))),
                "{line:?} must be malformed"
            );
        }
    }

    #[test]
    fn a_message_without_a_version_field_is_malformed() {
        assert!(matches!(
            parse_request(r#"{"id":1,"cmd":"status"}"#),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
