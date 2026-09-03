//! Application use cases and injectable orchestration ports.

use crate::AppError;
use crate::config::ExecutionRequest;
use crate::registry::PublishedImage;
use crate::runner::RunReport;
use crate::template::TemplatePlan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct CleanCandidate {
    pub task_id: String,
    pub repository_id: String,
    pub kind: ResourceKind,
    pub identifier: String,
    pub owner: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CleanPlan {
    pub candidates: Vec<CleanCandidate>,
    pub refused: Vec<String>,
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
