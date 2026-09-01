//! Infrastructure-independent specification and report for a one-shot verification job.

use crate::build::ImageRef;
use crate::config::Platform;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSpec {
    /// Caller-generated identifier. The Docker adapter uses it as an ownership label.
    pub task_id: String,
    pub image: ImageRef,
    pub platform: Platform,
    pub build: Vec<RunStep>,
    pub test: Vec<RunStep>,
    pub resources: RunResources,
    pub timeout_ms: u64,
    pub fail_fast: bool,
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
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub duration_ms: u64,
    pub status: RunStatus,
    pub steps: Vec<StepResult>,
    /// Cleanup errors are reported without hiding the primary job outcome.
    pub cleanup_error: Option<String>,
}
