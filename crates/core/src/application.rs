//! Application use cases and injectable orchestration ports.

use crate::AppError;
use crate::build::{ImageDigest, ImageRef};
use crate::config::ExecutionRequest;
use crate::registry::{PublishedImage, RemotePublicationFact};
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
    /// Runtime-only deadline; deliberately excluded from deterministic identity.
    pub deadline: Option<std::time::Instant>,
}

impl ExecutionPlan {
    /// Enforce verified-output semantics before entering infrastructure.
    pub fn validate_mode(&self, mode: WorkflowMode) -> Result<(), AppError> {
        if mode == WorkflowMode::Build && (self.request.push || self.request.oci_layout.is_some()) {
            return Err(AppError::Configuration(
                "--push and --oci-layout require verify mode".into(),
            ));
        }
        if self.request.push && self.request.oci_layout.is_some() {
            return Err(AppError::Configuration(
                "--push and --oci-layout must be requested in separate workflows".into(),
            ));
        }
        Ok(())
    }

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
        hash(
            &mut hasher,
            request.requested_git_ref.as_deref().unwrap_or(""),
        );
        hash(&mut hasher, request.git_ref.as_deref().unwrap_or(""));
        hash(
            &mut hasher,
            request.repository_config_digest.as_deref().unwrap_or(""),
        );
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
            deadline: None,
        }
    }
}

/// Stable identity of the exact repository configuration bytes used to plan.
pub fn configuration_source_digest(source: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(source))
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
    pub publication_progress: Vec<RemotePublicationFact>,
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
    let bytes = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    write_failure_bytes(&bytes, path, &FAILURE_SEQUENCE)
}

fn write_failure_bytes(bytes: &[u8], path: &Path, sequence: &AtomicU64) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("report.json");
    let (temporary, mut file) = loop {
        let temporary = parent.join(format!(
            ".{name}.failure.{}.{}.tmp",
            std::process::id(),
            sequence.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&temporary, path)
    })();
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
        plan.validate_mode(WorkflowMode::Build)?;
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
        plan.validate_mode(WorkflowMode::Verify)?;
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
    /// Exact journal snapshot revalidated after acquiring the exclusive lease.
    #[serde(skip)]
    pub journal_revisions: BTreeMap<PathBuf, Vec<String>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CleanResult {
    pub dry_run: bool,
    pub succeeded: Vec<CleanCandidate>,
    pub skipped: Vec<String>,
    /// Trusted resources that were already absent.
    pub absent: Vec<String>,
    /// Owned resources deliberately left in place (active, cancelled, or referenced).
    pub unfinished: Vec<String>,
    pub failed: Vec<String>,
}

impl CleanResult {
    pub fn complete(&self) -> bool {
        self.failed.is_empty() && self.unfinished.is_empty()
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

#[cfg(test)]
mod clean_result_tests {
    use super::CleanResult;

    #[test]
    fn dry_run_absent_and_excluded_are_complete_but_unfinished_is_not() {
        let complete = CleanResult {
            dry_run: true,
            skipped: vec!["dry-run or excluded".into()],
            absent: vec!["already absent".into()],
            ..CleanResult::default()
        };
        assert!(complete.complete());
        let incomplete = CleanResult {
            unfinished: vec!["active, cancelled, or referenced".into()],
            ..CleanResult::default()
        };
        assert!(!incomplete.complete());
    }
}

#[cfg(test)]
mod workflow_contract_tests {
    use super::*;
    use crate::config::{CliOverrides, Config};
    use crate::template::TemplateCatalog;

    fn plan() -> ExecutionPlan {
        let config = Config::parse_yaml(
            "version: 1\ntemplate:\n  id: rust-bazel\n  parameters:\n    platform: linux/amd64\n",
        )
        .unwrap();
        let request = ExecutionRequest::resolve(&config, CliOverrides::default());
        let template = TemplateCatalog::builtin()
            .unwrap()
            .plan(&config.template, request.platform)
            .unwrap();
        ExecutionPlan::new(template, request)
    }

    struct MustNotExecute;
    impl WorkflowPort for MustNotExecute {
        fn execute(&self, _: WorkflowMode, _: &ExecutionPlan) -> Result<WorkflowResult, AppError> {
            panic!("invalid output request reached infrastructure")
        }
    }

    #[test]
    fn output_contracts_reject_untested_and_combined_outputs_before_infrastructure() {
        let mut plan = plan();
        assert!(plan.validate_mode(WorkflowMode::Build).is_ok());
        plan.request.oci_layout = Some("layout".into());
        assert!(BuildUseCase::new(&MustNotExecute).execute(&plan).is_err());
        assert!(plan.validate_mode(WorkflowMode::Verify).is_ok());
        plan.request.push = true;
        assert!(VerifyUseCase::new(&MustNotExecute).execute(&plan).is_err());
        plan.request.oci_layout = None;
        assert!(BuildUseCase::new(&MustNotExecute).execute(&plan).is_err());
        assert!(plan.validate_mode(WorkflowMode::Verify).is_ok());
    }

    #[test]
    fn failure_writer_retries_stale_names_and_preserves_existing_reports() {
        let root = std::env::temp_dir().join(format!(
            "failure-report-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let destination = root.join("failure.json");
        let stale = root.join(format!(
            ".failure.json.failure.{}.0.tmp",
            std::process::id()
        ));
        fs::write(&stale, b"stale").unwrap();
        let sequence = AtomicU64::new(0);
        write_failure_bytes(b"{\"status\":\"failed\"}", &destination, &sequence).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"{\"status\":\"failed\"}");
        assert_eq!(fs::read(&stale).unwrap(), b"stale");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        let error = write_failure_bytes(b"replacement", &destination, &sequence).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"{\"status\":\"failed\"}");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(&root).unwrap();
    }
}
