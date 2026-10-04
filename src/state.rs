//! Group state: `groups.json` (spec §5).
//!
//! Membership only — never runtime state; active state is read live from
//! systemd (§2). Only ussd writes, only after a successful mutation,
//! atomically: a temp file in the same directory, then `rename(2)`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version of `groups.json` (v1).
pub const STATE_VERSION: u32 = 1;

/// Hard error loading or writing the state file (§5).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Corrupt or schema-violating file — a hard error, never a silent
    /// rewrite. ussd refuses to start on this (clear journal line).
    #[error("corrupt state file {path}: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    /// `version` present but not 1.
    #[error("unknown state version {version} in {path} (expected {STATE_VERSION})")]
    UnknownVersion { path: PathBuf, version: u32 },
    /// The in-memory state could not be serialized (internal failure).
    #[error("failed to serialize state: {reason}")]
    Serialize { reason: String },
    /// OS-level failure touching the state file.
    #[error("io error on state file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// On-disk schema (v1): membership only.
#[derive(Debug, Serialize, Deserialize)]
struct File {
    version: u32,
    /// Group name → members in **add order**.
    groups: BTreeMap<String, Vec<String>>,
}

/// Loaded group state. Group display order is alphabetical (BTreeMap);
/// member order within a group is add order (§5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Groups {
    groups: BTreeMap<String, Vec<String>>,
}

/// Result of [`Groups::add_member`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// The group did not exist; it was created with this member.
    Created,
    /// The group existed; the member was appended (add order preserved).
    Appended,
    /// The member was already in this group — a no-op (§4.2).
    AlreadyMember,
}

/// Result of [`Groups::remove_member`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The member was detached; the group still has other members.
    Removed,
    /// The last member was detached; the group was deleted (§5).
    LastMemberRemoved,
    /// The group is missing or does not contain the member.
    NotAMember,
}

impl Groups {
    /// Load from `path`.
    ///
    /// - Missing file → empty state (first use).
    /// - Corrupt / wrong-shape file → [`StateError::Corrupt`] (hard error).
    /// - `version` ≠ 1 → [`StateError::UnknownVersion`] (hard error).
    pub fn load(path: &Path) -> Result<Self, StateError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(path, &text),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(StateError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    fn parse(path: &Path, text: &str) -> Result<Self, StateError> {
        let file: File = serde_json::from_str(text).map_err(|e| StateError::Corrupt {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        if file.version != STATE_VERSION {
            return Err(StateError::UnknownVersion {
                path: path.to_path_buf(),
                version: file.version,
            });
        }
        Ok(Self {
            groups: file.groups,
        })
    }

    /// Atomically persist to `path` (§5): temp file in the same directory,
    /// then `rename(2)` over the target. The parent dir is created `0755` on
    /// demand; the file is `0644`.
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|e| StateError::Io {
                path: dir.clone(),
                source: e,
            })?;
            // Parent dir mode 0755 (§5), set at creation time.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).map_err(|e| {
                StateError::Io {
                    path: dir.clone(),
                    source: e,
                }
            })?;
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("groups.json");
        let tmp = dir.join(format!("{file_name}.tmp-{}", std::process::id()));

        // create_new: never clobber a pre-existing temp file.
        let mut options = fs::OpenOptions::new();
        let mut file = options
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(|e| StateError::Io {
                path: tmp.clone(),
                source: e,
            })?;
        let doc = File {
            version: STATE_VERSION,
            groups: self.groups.clone(),
        };
        let json = serde_json::to_string_pretty(&doc).map_err(|e| StateError::Serialize {
            reason: e.to_string(),
        })?;
        file.write_all(json.as_bytes()).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            StateError::Io {
                path: tmp.clone(),
                source: e,
            }
        })?;
        // Enforce 0644 regardless of umask: open(2) masks the requested mode.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            StateError::Io {
                path: tmp.clone(),
                source: e,
            }
        })?;
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            StateError::Io {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
        Ok(())
    }

    // -- reads -------------------------------------------------------------

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn has_group(&self, group: &str) -> bool {
        self.groups.contains_key(group)
    }

    /// Members of `group` in add order, if the group exists.
    pub fn members(&self, group: &str) -> Option<&[String]> {
        self.groups.get(group).map(Vec::as_slice)
    }

    /// Group names in alphabetical order.
    pub fn group_names(&self) -> Vec<&str> {
        self.groups.keys().map(String::as_str).collect()
    }

    /// Which group `service` belongs to, if any.
    pub fn group_of(&self, service: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|(_, members)| members.iter().any(|m| m == service))
            .map(|(group, _)| group.as_str())
    }

    // -- lifecycle (mutating; persist AFTER success, §5) --------------------

    /// Add a member; create the group if new. No-op for an existing member.
    /// Member order within the group is add order (§5).
    pub fn add_member(&mut self, group: &str, service: &str) -> AddOutcome {
        match self.groups.get_mut(group) {
            Some(members) if members.iter().any(|m| m == service) => AddOutcome::AlreadyMember,
            Some(members) => {
                members.push(service.to_owned());
                AddOutcome::Appended
            }
            None => {
                self.groups
                    .insert(group.to_owned(), vec![service.to_owned()]);
                AddOutcome::Created
            }
        }
    }

    /// Detach a member; delete the group when it was the last member (§5).
    pub fn remove_member(&mut self, group: &str, service: &str) -> RemoveOutcome {
        let Some(members) = self.groups.get_mut(group) else {
            return RemoveOutcome::NotAMember;
        };
        let Some(pos) = members.iter().position(|m| m == service) else {
            return RemoveOutcome::NotAMember;
        };
        members.remove(pos);
        if members.is_empty() {
            self.groups.remove(group);
            RemoveOutcome::LastMemberRemoved
        } else {
            RemoveOutcome::Removed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A unique temp dir per test, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("uss-state-test-{}-{}", std::process::id(), tag));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn state_path(tag: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new(tag);
        let path = dir.path().join("groups.json");
        (dir, path)
    }

    #[test]
    fn missing_file_is_empty_state() {
        let (_dir, path) = state_path("missing");
        let groups = Groups::load(&path).unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn round_trip_preserves_groups_member_order_and_version() {
        let (_dir, path) = state_path("round-trip");
        let mut groups = Groups::default();
        groups.add_member("vpn", "openvpn.service");
        groups.add_member("vpn", "wireguard.service");
        groups.add_member("dev", "foo.service");
        groups.save(&path).unwrap();

        let loaded = Groups::load(&path).unwrap();
        assert_eq!(loaded, groups);
        assert_eq!(
            loaded.group_names(),
            vec!["dev", "vpn"] // alphabetical
        );
        assert_eq!(
            loaded.members("vpn").unwrap(),
            vec!["openvpn.service", "wireguard.service"] // add order
        );

        // The on-disk document is schema v1 with the full *.service forms.
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["groups"]["dev"], serde_json::json!(["foo.service"]));
    }

    #[test]
    fn save_creates_parent_dir_0755_and_file_0644() {
        let dir = TempDir::new("modes");
        let path = dir.path().join("uss").join("groups.json");
        Groups::default().save(&path).unwrap();

        assert_eq!(
            fs::metadata(dir.path().join("uss"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let (dir, path) = state_path("atomic");
        let mut groups = Groups::default();
        groups.add_member("vpn", "openvpn.service");
        groups.save(&path).unwrap();
        groups.save(&path).unwrap(); // second write over the first

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["groups.json"]);
    }

    #[test]
    fn corrupt_file_is_a_hard_error() {
        let (_dir, path) = state_path("corrupt");
        fs::write(&path, "{ not json").unwrap();
        let err = Groups::load(&path).unwrap_err();
        assert!(matches!(err, StateError::Corrupt { .. }));

        // Wrong shape (members as an object) is corrupt too.
        fs::write(
            &path,
            r#"{"version":1,"groups":{"vpn":{"openvpn.service":true}}}"#,
        )
        .unwrap();
        assert!(matches!(
            Groups::load(&path).unwrap_err(),
            StateError::Corrupt { .. }
        ));

        // Load never rewrites the file.
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"version":1,"groups":{"vpn":{"openvpn.service":true}}}"#
        );
    }

    #[test]
    fn unknown_version_is_a_hard_error() {
        let (_dir, path) = state_path("version");
        fs::write(&path, r#"{"version":2,"groups":{}}"#).unwrap();
        let err = Groups::load(&path).unwrap_err();
        match err {
            StateError::UnknownVersion { path: p, version } => {
                assert_eq!(p, path);
                assert_eq!(version, 2);
            }
            other => panic!("expected UnknownVersion, got {other:?}"),
        }
    }

    #[test]
    fn group_lifecycle_create_and_delete() {
        let mut groups = Groups::default();

        assert_eq!(
            groups.add_member("vpn", "openvpn.service"),
            AddOutcome::Created
        );
        assert_eq!(
            groups.add_member("vpn", "wireguard.service"),
            AddOutcome::Appended
        );
        assert_eq!(
            groups.add_member("vpn", "openvpn.service"),
            AddOutcome::AlreadyMember
        );
        // No duplicate: the second add of the same member is a no-op.
        assert_eq!(groups.members("vpn").unwrap().len(), 2);

        assert_eq!(
            groups.remove_member("vpn", "wireguard.service"),
            RemoveOutcome::Removed
        );
        assert!(groups.has_group("vpn"));
        assert_eq!(
            groups.remove_member("vpn", "openvpn.service"),
            RemoveOutcome::LastMemberRemoved
        );
        assert!(!groups.has_group("vpn"));

        // Removing from a missing group / a non-member is NotAMember.
        assert_eq!(
            groups.remove_member("vpn", "openvpn.service"),
            RemoveOutcome::NotAMember
        );
        assert_eq!(
            groups.remove_member("dev", "foo.service"),
            RemoveOutcome::NotAMember
        );
    }

    #[test]
    fn group_of_finds_the_owning_group() {
        let mut groups = Groups::default();
        groups.add_member("dev", "foo.service");
        groups.add_member("vpn", "openvpn.service");
        assert_eq!(groups.group_of("foo.service"), Some("dev"));
        assert_eq!(groups.group_of("openvpn.service"), Some("vpn"));
        assert_eq!(groups.group_of("other.service"), None);
    }
}
