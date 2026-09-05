//! Infrastructure-independent OCI registry naming and reports.

use crate::build::{ImageDigest, ImageRef, PlatformDigest};
use crate::config::Platform;
use serde::Serialize;
use std::fmt::{self, Display, Formatter};

/// A fully-qualified OCI repository without a tag or digest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RegistryRepository(String);

impl RegistryRepository {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim() != value || value.contains(char::is_whitespace) {
            return Err("registry repository must be non-empty and contain no whitespace".into());
        }
        if value.contains("://") || value.contains('@') {
            return Err("registry repository must be an OCI name, not a URL or digest".into());
        }
        let (registry, path) = value.split_once('/').ok_or_else(|| {
            "registry repository must be fully qualified as registry/repository".to_owned()
        })?;
        if registry.is_empty() || path.is_empty() || path.split('/').any(str::is_empty) {
            return Err("registry repository contains an empty name component".into());
        }
        if let Some(last) = path.rsplit('/').next()
            && last.contains(':')
        {
            return Err("registry repository must not contain a tag".into());
        }
        let valid_component = |component: &str| {
            component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        };
        if !path.split('/').all(valid_component) {
            return Err(
                "OCI repository path must contain only lowercase letters, digits, '.', '_' or '-'"
                    .into(),
            );
        }
        if registry.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b':' | b'[' | b']'))
        }) {
            return Err("registry host contains unsupported characters".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn registry(&self) -> &str {
        self.0.split_once('/').expect("validated repository").0
    }

    pub fn tagged(&self, tag: &RegistryTag) -> ImageRef {
        ImageRef::new(format!("{}:{}", self.0, tag.0)).expect("validated OCI name and tag")
    }
}

impl Display for RegistryRepository {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A validated OCI tag. Mutable aliases and immutable content tags share this syntax.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RegistryTag(String);

impl RegistryTag {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let mut bytes = value.bytes();
        let first = bytes
            .next()
            .ok_or_else(|| "registry tag must not be empty".to_owned())?;
        if value.len() > 128
            || !(first.is_ascii_alphanumeric() || first == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err("registry tag must match [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The default immutable tag derived solely from content identity.
    pub fn for_digest(digest: &ImageDigest) -> Self {
        Self(format!(
            "sha256-{}",
            digest
                .as_str()
                .strip_prefix("sha256:")
                .expect("validated digest")
                .to_ascii_lowercase()
        ))
    }
}

impl Display for RegistryTag {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRequest {
    pub source: ImageRef,
    pub repository: RegistryRepository,
    pub digest: ImageDigest,
    pub platform_digests: Vec<PlatformDigest>,
    /// Optional mutable conveniences such as `latest`; never replaces the content tag.
    pub aliases: Vec<RegistryTag>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub image: ImageRef,
    pub expected_digest: ImageDigest,
    pub expected_platforms: Vec<Platform>,
}

/// Safe to serialize: credentials are runtime-only adapter inputs and cannot enter reports.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublishedImage {
    pub immutable: ImageRef,
    pub aliases: Vec<ImageRef>,
    pub digest: ImageDigest,
    pub platform_digests: Vec<PlatformDigest>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationFactKind {
    RegistryPreflightStaging,
    EnvironmentStaging,
    TaskStaging,
    TaskIndexStaging,
    Immutable,
    Alias,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationFinality {
    Staging,
    Final,
}

/// A remote side effect attempted or observed during publication.
/// An unverified fact never establishes the final immutable/alias contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemotePublicationFact {
    pub kind: PublicationFactKind,
    pub reference: ImageRef,
    /// Absent when a remote write may have committed but inspection failed.
    pub digest: Option<ImageDigest>,
    pub verified: bool,
    pub finality: PublicationFinality,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PulledImage {
    pub image: ImageRef,
    pub digest: ImageDigest,
    pub platforms: Vec<Platform>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_and_tag_names_are_unambiguous() {
        let repository = RegistryRepository::new("localhost:5000/team/image").unwrap();
        assert_eq!(repository.registry(), "localhost:5000");
        assert_eq!(
            repository
                .tagged(&RegistryTag::new("stable").unwrap())
                .as_str(),
            "localhost:5000/team/image:stable"
        );
        for invalid in [
            "image",
            "https://registry.test/team/image",
            "registry.test/Team/image",
            "registry.test/team/image:latest",
            "registry.test//image",
        ] {
            assert!(RegistryRepository::new(invalid).is_err(), "{invalid}");
        }
        for invalid in ["", ".bad", "bad tag", &"x".repeat(129)] {
            assert!(RegistryTag::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn immutable_tag_is_content_addressed() {
        let digest = ImageDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(
            RegistryTag::for_digest(&digest).as_str(),
            format!("sha256-{}", "a".repeat(64))
        );
    }
}
