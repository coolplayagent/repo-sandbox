//! Stable identities and metadata for images that combine an environment with source.

use crate::build::ImageDigest;
use crate::snapshot::{SnapshotOrigin, SourceSnapshot};
use sha2::{Digest, Sha256};
use std::fmt::{self, Display, Formatter};

/// Fixed source location in every task image. Task images intentionally define no entrypoint.
pub const TASK_WORKDIR: &str = "/workspace";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationDigest(String);

impl ConfigurationDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let hex = value.strip_prefix("sha256:").unwrap_or(&value);
        if hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(hex.to_owned()))
        } else {
            Err("configuration digest must contain 64 lowercase SHA-256 digits".to_owned())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn oci_value(&self) -> String {
        format!("sha256:{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskImageIdentity(String);

impl TaskImageIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Docker tag derived only from immutable inputs, never wall-clock time.
    pub fn tag(&self) -> String {
        format!("sha256-{}", self.0)
    }

    pub fn oci_value(&self) -> String {
        format!("sha256:{}", self.0)
    }
}

impl Display for TaskImageIdentity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub struct TaskImageInputs<'a> {
    pub environment_digest: &'a ImageDigest,
    pub snapshot: &'a SourceSnapshot,
    pub template_id: &'a str,
    pub template_version: &'a str,
    pub configuration_digest: &'a ConfigurationDigest,
    /// Ownership boundary for daemon-global image tags and labels.
    pub repository_id: &'a str,
    /// Reproducible OCI creation time. It participates in identity so one tag
    /// can never name two image configurations with different timestamps.
    pub created: &'a str,
}

/// Versioned, length-delimited content identity. Field boundaries cannot collide.
pub fn task_image_identity(inputs: &TaskImageInputs<'_>) -> TaskImageIdentity {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "repo-sandbox-task-image-v2");
    hash_field(&mut hasher, inputs.environment_digest.as_str());
    hash_field(&mut hasher, inputs.snapshot.id.as_str());
    hash_field(
        &mut hasher,
        source_commit(inputs.snapshot).unwrap_or("local"),
    );
    hash_field(&mut hasher, inputs.template_id);
    hash_field(&mut hasher, inputs.template_version);
    hash_field(&mut hasher, inputs.configuration_digest.as_str());
    hash_field(&mut hasher, inputs.repository_id);
    hash_field(&mut hasher, inputs.created);
    TaskImageIdentity(format!("{:x}", hasher.finalize()))
}

pub fn source_commit(snapshot: &SourceSnapshot) -> Option<&str> {
    match &snapshot.origin {
        SnapshotOrigin::Local { .. } => None,
        SnapshotOrigin::RemoteGit { commit, .. } => Some(commit.as_str()),
    }
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{SnapshotId, SnapshotOrigin};
    use std::path::PathBuf;

    fn snapshot(digit: char) -> SourceSnapshot {
        SourceSnapshot {
            id: SnapshotId::parse(digit.to_string().repeat(64)).unwrap(),
            origin: SnapshotOrigin::Local {
                canonical_root: PathBuf::from("source"),
            },
            file_count: 1,
            recurse_submodules: false,
        }
    }

    fn identity(snapshot: &SourceSnapshot, config: &ConfigurationDigest) -> TaskImageIdentity {
        let environment = ImageDigest::new(format!("sha256:{}", "e".repeat(64))).unwrap();
        task_image_identity(&TaskImageInputs {
            environment_digest: &environment,
            snapshot,
            template_id: "rust-bazel",
            template_version: "1.0.0",
            configuration_digest: config,
            repository_id: "repository-a",
            created: "2026-09-01T00:00:00Z",
        })
    }

    #[test]
    fn identical_inputs_have_the_same_immutable_tag() {
        let source = snapshot('a');
        let config = ConfigurationDigest::parse("c".repeat(64)).unwrap();
        assert_eq!(identity(&source, &config), identity(&source, &config));
        assert!(identity(&source, &config).tag().starts_with("sha256-"));
    }

    #[test]
    fn source_and_configuration_changes_have_new_identities() {
        let config = ConfigurationDigest::parse("c".repeat(64)).unwrap();
        let changed_config = ConfigurationDigest::parse("d".repeat(64)).unwrap();
        assert_ne!(
            identity(&snapshot('a'), &config),
            identity(&snapshot('b'), &config)
        );
        assert_ne!(
            identity(&snapshot('a'), &config),
            identity(&snapshot('a'), &changed_config)
        );
    }

    #[test]
    fn configuration_digests_are_normalized() {
        let bare = ConfigurationDigest::parse("a".repeat(64)).unwrap();
        let prefixed = ConfigurationDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(bare, prefixed);
        assert!(ConfigurationDigest::parse("A".repeat(64)).is_err());
    }

    #[test]
    fn different_creation_metadata_cannot_share_an_identity_tag() {
        let source = snapshot('a');
        let config = ConfigurationDigest::parse("c".repeat(64)).unwrap();
        let environment = ImageDigest::new(format!("sha256:{}", "e".repeat(64))).unwrap();
        let inputs = |created| TaskImageInputs {
            environment_digest: &environment,
            snapshot: &source,
            template_id: "rust-bazel",
            template_version: "1.0.0",
            configuration_digest: &config,
            repository_id: "repository-a",
            created,
        };
        assert_ne!(
            task_image_identity(&inputs("2026-09-01T00:00:00Z")),
            task_image_identity(&inputs("2026-09-01T00:00:01Z"))
        );
    }

    #[test]
    fn different_repository_owners_cannot_share_a_daemon_tag() {
        let source = snapshot('a');
        let config = ConfigurationDigest::parse("c".repeat(64)).unwrap();
        let environment = ImageDigest::new(format!("sha256:{}", "e".repeat(64))).unwrap();
        let identity = |repository_id| {
            task_image_identity(&TaskImageInputs {
                environment_digest: &environment,
                snapshot: &source,
                template_id: "rust-bazel",
                template_version: "1.0.0",
                configuration_digest: &config,
                repository_id,
                created: "2026-09-01T00:00:00Z",
            })
        };
        assert_ne!(identity("repository-a"), identity("repository-b"));
    }
}
