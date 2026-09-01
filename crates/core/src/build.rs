//! Infrastructure-independent results produced by an environment image build.

use std::fmt::{self, Display, Formatter};

use crate::config::Platform;
use serde::Serialize;

/// The OCI image name selected by the caller (for example `example/image:tag`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
            Err("image reference must be non-empty and contain no whitespace".to_owned())
        } else {
            Ok(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ImageRef {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A content digest returned by BuildKit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ImageDigest(String);

impl ImageDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let valid = value
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if valid {
            Ok(Self(value))
        } else {
            Err("image digest must be a sha256 digest with 64 hexadecimal digits".to_owned())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ImageDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BuiltImage {
    pub image: ImageRef,
    /// Digest of the exported image or multi-platform image index.
    pub digest: ImageDigest,
    /// Concrete image manifest digest for every requested OCI platform.
    pub platform_digests: Vec<PlatformDigest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformDigest {
    pub platform: Platform,
    pub digest: ImageDigest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_identity_types_reject_ambiguous_values() {
        assert!(ImageRef::new("registry.test/repo:tag").is_ok());
        assert!(ImageRef::new("bad image").is_err());
        assert!(ImageDigest::new(format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(ImageDigest::new("sha256:short").is_err());
    }

    #[test]
    fn build_results_can_report_each_oci_platform_digest() {
        let digest = ImageDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap();
        let built = BuiltImage {
            image: ImageRef::new("registry.test/repo:tag").unwrap(),
            digest: digest.clone(),
            platform_digests: vec![PlatformDigest {
                platform: Platform::LinuxArm64,
                digest: digest.clone(),
            }],
        };
        assert_eq!(built.platform_digests[0].platform, Platform::LinuxArm64);
        let report = serde_yaml::to_string(&built).unwrap();
        assert!(report.contains("linux/arm64"));
        assert!(report.contains(digest.as_str()));
    }
}
