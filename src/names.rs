//! Naming and validation (spec §10).
//!
//! CLI service names normalize to the full `*.service` form; group names are
//! validated as-is. Both are checked locally (no D-Bus) before anything else,
//! so failures are usage errors (exit 1).

use crate::error::Error;

/// Maximum length (bytes) of a normalized service unit name.
const SERVICE_NAME_MAX_LEN: usize = 255;

/// Maximum length (bytes) of a group name.
const GROUP_NAME_MAX_LEN: usize = 64;

const SERVICE_SUFFIX: &str = ".service";

/// Normalize + validate a CLI service name (§10).
///
/// - `foo` → `foo.service`
/// - `foo.service` → accepted verbatim
/// - anything else (other suffixes, empty, multiple dots not ending in
///   `.service`) → `InvalidServiceName` (exit 1)
///
/// After normalization, a light local syntax check: no leading `-`, no NUL or
/// whitespace, length ≤ 255 — before any D-Bus call.
pub fn normalize_service(input: &str) -> Result<String, Error> {
    let normalized = if input.ends_with(SERVICE_SUFFIX) {
        // Verbatim; an empty prefix (".service") is rejected below.
        input.to_owned()
    } else if !input.contains('.') {
        format!("{input}{SERVICE_SUFFIX}")
    } else {
        return Err(Error::InvalidServiceName {
            input: input.to_owned(),
        });
    };

    // A normalized name is always `<prefix>.service`; the empty prefix (the
    // bare suffix ".service") is rejected by requiring one byte before it.
    let has_nonempty_prefix = normalized.len() > SERVICE_SUFFIX.len();
    if normalized.starts_with('-')
        || !has_nonempty_prefix
        || normalized.len() > SERVICE_NAME_MAX_LEN
        || normalized.contains('\0')
        || normalized.chars().any(char::is_whitespace)
    {
        return Err(Error::InvalidServiceName {
            input: input.to_owned(),
        });
    }
    Ok(normalized)
}

/// Validate a group name (§10): non-empty, ≤ 64 bytes, no whitespace, no `/`,
/// no NUL. Group names are local file keys only — no systemd interpretation.
pub fn validate_group(input: &str) -> Result<(), Error> {
    let invalid = || Error::InvalidGroupName {
        input: input.to_owned(),
    };

    if input.is_empty()
        || input.len() > GROUP_NAME_MAX_LEN
        || input.contains('/')
        || input.contains('\0')
        || input.chars().any(char::is_whitespace)
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_gets_the_service_suffix() {
        assert_eq!(normalize_service("foo").unwrap(), "foo.service");
        assert_eq!(normalize_service("openvpn").unwrap(), "openvpn.service");
    }

    #[test]
    fn verbatim_service_name_is_accepted() {
        assert_eq!(normalize_service("foo.service").unwrap(), "foo.service");
        // Multiple dots are fine when the name ends in `.service`
        // (e.g. template units like `foo@bar.service`).
        assert_eq!(
            normalize_service("foo@bar.service").unwrap(),
            "foo@bar.service"
        );
    }

    #[test]
    fn other_suffixes_are_rejected() {
        for input in ["foo.target", "foo.socket", "foo.path", "foo.SERVICE"] {
            let err = normalize_service(input).unwrap_err();
            assert_eq!(
                err,
                Error::InvalidServiceName {
                    input: input.into()
                }
            );
        }
    }

    #[test]
    fn dotted_names_without_the_service_suffix_are_rejected() {
        for input in ["foo.bar", ".service", "foo.tar.gz", "a.b.c"] {
            assert!(
                normalize_service(input).is_err(),
                "{input} must be rejected"
            );
        }
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(normalize_service("").is_err());
    }

    #[test]
    fn light_syntax_checks_run_after_normalization() {
        // Leading dash survives into the normalized name and is rejected.
        assert!(normalize_service("-foo").is_err());
        // Whitespace (no dot → normalized, then caught by the syntax check).
        assert!(normalize_service("foo bar").is_err());
        assert!(normalize_service("foo\tbar").is_err());
        // NUL byte.
        assert!(normalize_service("foo\0bar").is_err());
        // Over the 255-byte limit only after normalization.
        let long = "a".repeat(255);
        assert!(normalize_service(&long).is_err());
        // Exactly at the limit is fine: 247 a's + ".service" = 255.
        let at_limit = "a".repeat(247);
        assert_eq!(
            normalize_service(&at_limit).unwrap(),
            format!("{at_limit}.service")
        );
    }

    #[test]
    fn group_names_validate_per_10() {
        assert!(validate_group("vpn").is_ok());
        assert!(validate_group("dev-box").is_ok());
        // 64 bytes exactly is fine.
        assert!(validate_group(&"g".repeat(64)).is_ok());
        // 65 bytes is not.
        assert!(validate_group(&"g".repeat(65)).is_err());
        // Multi-byte characters count in BYTES.
        assert!(validate_group(&"é".repeat(32)).is_ok()); // 64 bytes
        assert!(validate_group(&"é".repeat(33)).is_err()); // 66 bytes
        assert!(validate_group("").is_err());
        assert!(validate_group("a/b").is_err());
        assert!(validate_group("a b").is_err());
        assert!(validate_group("a\nb").is_err());
        assert!(validate_group("a\0b").is_err());
    }

    #[test]
    fn group_error_message_carries_the_input() {
        let err = validate_group("bad name").unwrap_err();
        assert_eq!(err.to_string(), "invalid group name: 'bad name'");
    }
}
