//! Infrastructure-independent specification and report for a one-shot verification job.

use crate::build::{ImageDigest, ImageRef};
use crate::config::{Config, Platform};
use crate::snapshot::SourceSnapshot;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSpec {
    /// Caller-generated identifier. The Docker adapter uses it as an ownership label.
    pub task_id: String,
    pub image: ImageRef,
    pub image_digest: ImageDigest,
    pub source_snapshot: SourceSnapshot,
    pub config_summary: ConfigSummary,
    pub platform: Platform,
    pub build: Vec<RunStep>,
    pub test: Vec<RunStep>,
    pub resources: RunResources,
    pub timeout_ms: u64,
    pub fail_fast: bool,
    /// When set, export every declared artifact directory beneath this root.
    pub artifact_export_root: Option<PathBuf>,
    /// Retain only task-owned diagnostics when the job fails.
    pub keep_on_failure: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigSummary {
    pub template_id: String,
    pub platform: Platform,
    pub build_steps: Vec<String>,
    pub test_steps: Vec<String>,
    pub artifact_directories: Vec<PathBuf>,
}

impl ConfigSummary {
    /// Produce a deliberately secret-free, stable subset of the validated config.
    pub fn from_config(config: &Config, platform: Platform) -> Self {
        Self {
            template_id: config.template.id.clone(),
            platform,
            build_steps: config.build.iter().map(|step| step.name.clone()).collect(),
            test_steps: config.test.iter().map(|step| step.name.clone()).collect(),
            artifact_directories: config
                .legacy
                .as_ref()
                .map(|legacy| legacy.artifacts.directories.clone())
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunStep {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunResources {
    pub cpu_count: u16,
    pub memory_mb: u32,
    pub temporary_storage_mb: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Build,
    Test,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimit {
    Memory,
    TemporaryStorage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepStatus {
    Succeeded,
    CommandFailed,
    TimedOut,
    ResourceExceeded { limit: ResourceLimit },
    InfrastructureFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StepResult {
    pub phase: StepPhase,
    pub name: String,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub status: StepStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    CommandFailed {
        phase: StepPhase,
        step: String,
        exit_code: Option<i32>,
    },
    TimedOut,
    ResourceExceeded {
        phase: StepPhase,
        step: String,
        limit: ResourceLimit,
    },
    InfrastructureFailed {
        operation: String,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunReport {
    pub task_id: String,
    pub container_id: Option<String>,
    pub source_snapshot: SourceSnapshot,
    pub config: ConfigSummary,
    pub image: ImageRef,
    pub image_digest: ImageDigest,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub duration_ms: u64,
    pub status: RunStatus,
    pub steps: Vec<StepResult>,
    pub exported_artifacts: Vec<PathBuf>,
    pub artifact_error: Option<String>,
    pub cleanup: CleanupResult,
    /// Cleanup errors are reported without hiding the primary job outcome.
    pub cleanup_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupResult {
    NotNeeded,
    Removed,
    RetainedOnFailure,
    Failed,
}

/// Serialize a report for either a successful or failed run. The parent is
/// created first and a same-directory rename prevents partially written JSON.
pub fn write_report_json(report: &RunReport, path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(&temporary, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)
}
