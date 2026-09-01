use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;

/// A reference to secret material owned by the host. The value itself never
/// becomes part of the configuration or snapshot domain model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalSecret {
    Environment(String),
    File(PathBuf),
}

/// Authentication used only while Git contacts a remote.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum GitAuthentication {
    #[default]
    None,
    SshAgent {
        known_hosts: Option<PathBuf>,
    },
    SshKey {
        private_key: PathBuf,
        known_hosts: Option<PathBuf>,
    },
    HttpsToken {
        username: String,
        token: ExternalSecret,
    },
    HttpsCredentialHelper,
}

/// A source supplied by the operator. Remote credentials are intentionally not
/// represented in the v1 domain model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceSpec {
    LocalDirectory(PathBuf),
    RemoteGit { repository: String, git_ref: String },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotOptions {
    pub recurse_submodules: bool,
    pub cleanup: CleanupPolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CleanupPolicy {
    /// Delete both successful and failed materializations when their guard is dropped.
    #[default]
    Delete,
    /// Preserve a successful materialization for a later task stage.
    Keep,
}

/// Lowercase SHA-256 identity of the normalized path/mode/content manifest.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn parse(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(SnapshotError::InvalidInput(
                "snapshot id must be a lowercase SHA-256 digest".into(),
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for SnapshotId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The immutable commit selected for a remote input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSha(String);

impl CommitSha {
    pub fn parse(value: impl Into<String>) -> Result<Self, SnapshotError> {
        let value = value.into();
        if (value.len() == 40 || value.len() == 64)
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(SnapshotError::InvalidInput(
                "resolved commit is not a full object id".into(),
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotOrigin {
    Local {
        canonical_root: PathBuf,
    },
    RemoteGit {
        repository: String,
        requested_ref: String,
        commit: CommitSha,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    pub id: SnapshotId,
    pub origin: SnapshotOrigin,
    pub file_count: usize,
    pub recurse_submodules: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SnapshotError {
    InvalidInput(String),
    Unsupported(String),
    Git(String),
    Authentication(String),
    Network(String),
    RepositoryNotFound(String),
    PermissionDenied(String),
    Io(String),
}

impl Display for SnapshotError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid source: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported source: {message}"),
            Self::Git(message) => write!(formatter, "git operation failed: {message}"),
            Self::Authentication(message) => {
                write!(formatter, "Git authentication failed: {message}")
            }
            Self::Network(message) => write!(formatter, "Git network failed: {message}"),
            Self::RepositoryNotFound(message) => {
                write!(formatter, "Git repository not found: {message}")
            }
            Self::PermissionDenied(message) => {
                write!(formatter, "Git permission denied: {message}")
            }
            Self::Io(message) => write!(formatter, "snapshot I/O failed: {message}"),
        }
    }
}

impl Error for SnapshotError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_require_full_normalized_digests() {
        assert!(SnapshotId::parse("a".repeat(64)).is_ok());
        assert!(SnapshotId::parse("A".repeat(64)).is_err());
        assert!(CommitSha::parse("A".repeat(40)).is_ok());
        assert!(CommitSha::parse("abc").is_err());
    }

    #[test]
    fn authentication_models_only_external_secret_references() {
        let auth = GitAuthentication::HttpsToken {
            username: "git".into(),
            token: ExternalSecret::Environment("REPO_TOKEN".into()),
        };
        assert!(!format!("{auth:?}").contains("actual-token"));
    }
}
