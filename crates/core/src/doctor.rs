//! Stable host capability model shared by doctor and future execution paths.

use serde::{Deserialize, Serialize};

/// A prerequisite that can be consumed by build and verify planning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    OperatingSystem,
    CpuArchitecture,
    DockerDaemon,
    Buildkit,
    Buildx,
    QemuBinfmt,
    DiskSpace,
    RegistryConnectivity,
}

/// Whether a capability is usable for sandbox work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    Unavailable,
}

/// One independently evaluated host prerequisite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capability {
    pub kind: CapabilityKind,
    pub status: CapabilityStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub remediation: Vec<String>,
}

impl Capability {
    pub fn available(kind: CapabilityKind, summary: impl Into<String>) -> Self {
        Self {
            kind,
            status: CapabilityStatus::Available,
            summary: summary.into(),
            remediation: Vec::new(),
        }
    }

    pub fn unavailable(
        kind: CapabilityKind,
        summary: impl Into<String>,
        remediation: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            kind,
            status: CapabilityStatus::Unavailable,
            summary: summary.into(),
            remediation: remediation.into_iter().map(Into::into).collect(),
        }
    }
}

/// Aggregate readiness derived solely from individual capability conclusions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ready,
    NotReady,
}

/// Complete, serializable result of a read-only doctor run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DoctorReport {
    pub status: DoctorStatus,
    pub capabilities: Vec<Capability>,
}

impl DoctorReport {
    pub fn from_capabilities(capabilities: Vec<Capability>) -> Self {
        let status = if capabilities
            .iter()
            .all(|capability| capability.status == CapabilityStatus::Available)
        {
            DoctorStatus::Ready
        } else {
            DoctorStatus::NotReady
        };
        Self {
            status,
            capabilities,
        }
    }

    pub const fn is_ready(&self) -> bool {
        matches!(self.status, DoctorStatus::Ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_status_is_derived_from_capabilities() {
        let ready = DoctorReport::from_capabilities(vec![Capability::available(
            CapabilityKind::DockerDaemon,
            "running",
        )]);
        assert!(ready.is_ready());

        let not_ready = DoctorReport::from_capabilities(vec![Capability::unavailable(
            CapabilityKind::DockerDaemon,
            "not running",
            ["start Docker"],
        )]);
        assert!(!not_ready.is_ready());
    }
}
