//! Application use cases and injectable orchestration ports.

use crate::AppError;
use crate::build::{ImageDigest, ImageRef};
use crate::config::ExecutionRequest;
use crate::registry::PublishedImage;
use crate::runner::{CleanupResult, ConfigSummary, RunReport, StepResult};
use crate::snapshot::SourceSnapshot;
use crate::template::TemplatePlan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowMode {
    Build,
    Verify,
}

/// Complete immutable input handed to infrastructure. Its digest includes the
/// template graph, execution profile, and the finite CLI overrides.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPlan {
    pub template: TemplatePlan,
    pub request: ExecutionRequest,
    pub digest: String,
}

impl ExecutionPlan {
    pub fn new(template: TemplatePlan, request: ExecutionRequest) -> Self {
        let mut hasher = Sha256::new();
        hash(&mut hasher, "repo-sandbox-execution-plan-v1");
        hash(
            &mut hasher,
            &serde_json::to_string(&template).expect("serializable template plan"),
        );
        hash(&mut hasher, &request.platform.to_string());
        for platform in &request.platforms {
            hash(&mut hasher, &platform.to_string());
        }
        hash(
            &mut hasher,
            request
                .oci_layout
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref()
                .unwrap_or(""),
        );
        hash(&mut hasher, request.repository.as_deref().unwrap_or("."));
        hash(&mut hasher, request.git_ref.as_deref().unwrap_or(""));
        hash(
            &mut hasher,
            request
                .report
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref()
                .unwrap_or(""),
        );
        hash(&mut hasher, if request.push { "push" } else { "local" });
        hash(
            &mut hasher,
            if request.keep_on_failure {
                "keep"
            } else {
                "cleanup"
            },
        );
        hash(
            &mut hasher,
            if request.recurse_submodules {
                "submodules"
            } else {
                "no-submodules"
            },
        );
        hash(
            &mut hasher,
            request.remote_auth.https_username.as_deref().unwrap_or(""),
        );
        hash(
            &mut hasher,
            request
                .remote_auth
                .https_token_environment
                .as_deref()
                .unwrap_or(""),
        );
        hash(
            &mut hasher,
            if request.remote_auth.https_credential_helper {
                "https-helper"
            } else {
                "no-https-helper"
            },
        );
        hash(
            &mut hasher,
            request
                .remote_auth
                .ssh_private_key
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref()
                .unwrap_or(""),
        );
        hash(
            &mut hasher,
            request
                .remote_auth
                .ssh_known_hosts
                .as_ref()
                .map(|path| path.to_string_lossy())
                .as_deref()
                .unwrap_or(""),
        );
        hash(
            &mut hasher,
            if request.remote_auth.ssh_agent {
                "ssh-agent"
            } else {
                "no-ssh-agent"
            },
        );
        Self {
            template,
            request,
            digest: format!("sha256:{:x}", hasher.finalize()),
        }
    }
}

fn hash(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowResult {
    pub plan_digest: String,
    pub report: RunReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<PublishedImage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowFailureReport {
    pub schema_version: u8,
    pub task_id: String,
    pub plan_digest: String,
    pub phase: String,
    pub exit_code: i32,
    pub message: String,
    pub cleanup: CleanupResult,
    pub published: Option<PublishedImage>,
    pub container_id: Option<String>,
    pub source_snapshot: Option<SourceSnapshot>,
    pub config: Option<ConfigSummary>,
    pub image: Option<ImageRef>,
    pub image_digest: Option<ImageDigest>,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub duration_ms: u64,
    pub status: WorkflowFailureStatus,
    pub steps: Vec<StepResult>,
    pub exported_artifacts: Vec<PathBuf>,
    pub artifact_error: Option<String>,
    pub cleanup_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowFailureStatus {
    pub status: &'static str,
    pub operation: String,
    pub message: String,
}

static FAILURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn write_failure_report(report: &WorkflowFailureReport, path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("report.json");
    let temporary = parent.join(format!(
        ".{name}.failure.{}.{}.tmp",
        std::process::id(),
        FAILURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    let result = fs::hard_link(&temporary, path);
    let _ = fs::remove_file(temporary);
    result
}

pub trait WorkflowPort {
    fn execute(&self, mode: WorkflowMode, plan: &ExecutionPlan)
    -> Result<WorkflowResult, AppError>;
}

pub struct BuildUseCase<'a, P: ?Sized> {
    port: &'a P,
}
impl<'a, P: WorkflowPort + ?Sized> BuildUseCase<'a, P> {
    pub const fn new(port: &'a P) -> Self {
        Self { port }
    }
    pub fn execute(&self, plan: &ExecutionPlan) -> Result<WorkflowResult, AppError> {
        self.port.execute(WorkflowMode::Build, plan)
    }
}

pub struct VerifyUseCase<'a, P: ?Sized> {
    port: &'a P,
}
impl<'a, P: WorkflowPort + ?Sized> VerifyUseCase<'a, P> {
    pub const fn new(port: &'a P) -> Self {
        Self { port }
    }
    pub fn execute(&self, plan: &ExecutionPlan) -> Result<WorkflowResult, AppError> {
        self.port.execute(WorkflowMode::Verify, plan)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanRequest {
    pub repository: PathBuf,
    pub all: bool,
    pub include_images: bool,
    pub include_cache: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Container,
    Source,
    Builder,
    Image,
    Cache,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    #[default]
    Registered,
    Retained,
    Cleaned,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CleanCandidate {
    pub task_id: String,
    pub repository_id: String,
    pub kind: ResourceKind,
    pub identifier: String,
    pub owner: String,
    #[serde(default)]
    pub state: ResourceState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CleanPlan {
    pub candidates: Vec<CleanCandidate>,
    pub refused: Vec<String>,
    #[serde(skip)]
    pub manifest_root: Option<PathBuf>,
    #[serde(skip)]
    pub journal_roots: BTreeMap<String, PathBuf>,
    /// Advisory OS lock shared by workflows and held exclusively by cleanup.
    #[serde(skip)]
    pub lease_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CleanResult {
    pub dry_run: bool,
    pub succeeded: Vec<CleanCandidate>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

impl CleanResult {
    pub fn complete(&self) -> bool {
        self.failed.is_empty()
    }
}

pub trait CleanPort {
    fn plan(&self, request: &CleanRequest) -> Result<CleanPlan, AppError>;
    fn execute(&self, plan: &CleanPlan, dry_run: bool) -> Result<CleanResult, AppError>;
}

pub struct CleanUseCase<'a, P: ?Sized> {
    port: &'a P,
}
impl<'a, P: CleanPort + ?Sized> CleanUseCase<'a, P> {
    pub const fn new(port: &'a P) -> Self {
        Self { port }
    }
    pub fn plan(&self, request: &CleanRequest) -> Result<CleanPlan, AppError> {
        self.port.plan(request)
    }
    pub fn execute(&self, plan: &CleanPlan, dry_run: bool) -> Result<CleanResult, AppError> {
        self.port.execute(plan, dry_run)
    }
}
