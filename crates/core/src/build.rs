//! Infrastructure-independent results produced by an environment image build.

use std::fmt::{self, Display, Formatter};

/// The OCI image name selected by the caller (for example `example/image:tag`).
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltImage {
    pub image: ImageRef,
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
}
