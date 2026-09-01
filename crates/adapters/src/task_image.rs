//! BuildKit adapter for immutable task images containing a source snapshot.

use crate::buildkit::{
    BuildKit, BuildOptions, BuildRequest, Builder, CacheConfig, Cancellation, ImageOutput,
    ProcessExecutor, Progress,
};
use crate::snapshot::MaterializedSnapshot;
use repo_sandbox_core::build::{BuiltImage, ImageRef};
use repo_sandbox_core::config::Platform;
use repo_sandbox_core::snapshot::SnapshotError;
use repo_sandbox_core::task_image::{
    ConfigurationDigest, TaskImageIdentity, TaskImageInputs, source_commit, task_image_identity,
};
use repo_sandbox_core::template::TemplatePlan;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const DOCKERIGNORE: &str = r#"**
!Dockerfile
!.dockerignore
!source/
!source/**
source/**/.git
source/**/.git/**
source/**/.env
source/**/.env.*
source/**/.netrc
source/**/_netrc
source/**/id_rsa
source/**/id_ed25519
source/**/credentials
source/**/.docker/config.json
source/**/.git-credentials
source/**/.npmrc
source/**/.pypirc
source/**/.ssh
source/**/.ssh/**
source/**/.aws
source/**/.aws/**
"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskImageOptions {
    pub progress: Progress,
    pub cache: CacheConfig,
    pub builder: Builder,
}

impl Default for TaskImageOptions {
    fn default() -> Self {
        Self {
            progress: Progress::Auto,
            cache: CacheConfig::default(),
            builder: Builder::default(),
        }
    }
}

pub struct TaskImageRequest<'a> {
    pub environment: &'a BuiltImage,
    pub materialized: &'a MaterializedSnapshot,
    pub template_id: &'a str,
    pub template_version: &'a str,
    pub platform: Platform,
    pub configuration_digest: &'a ConfigurationDigest,
    /// OCI `org.opencontainers.image.created`, supplied by the orchestration clock.
    pub created: &'a str,
    /// Repository without a tag; the adapter appends the immutable content tag.
    pub repository: &'a str,
    pub options: TaskImageOptions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltTaskImage {
    pub image: BuiltImage,
    pub identity: TaskImageIdentity,
}

#[derive(Debug)]
pub enum TaskImageError {
    InvalidRequest(String),
    Context {
        operation: &'static str,
        source: io::Error,
    },
    Snapshot(SnapshotError),
    Build(crate::buildkit::BuildError),
}

impl Display for TaskImageError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid task image request: {message}")
            }
            Self::Context { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Snapshot(error) => Display::fmt(error, formatter),
            Self::Build(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for TaskImageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Context { source, .. } => Some(source),
            Self::Build(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::InvalidRequest(_) => None,
        }
    }
}

pub struct TaskImageBuilder<E> {
    buildkit: BuildKit<E>,
}

impl<E> TaskImageBuilder<E> {
    pub const fn new(executor: E) -> Self {
        Self {
            buildkit: BuildKit::new(executor),
        }
    }
}

impl<E: ProcessExecutor> TaskImageBuilder<E> {
    pub fn build(
        &self,
        request: TaskImageRequest<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<BuiltTaskImage, TaskImageError> {
        validate_request(&request)?;
        let identity = task_image_identity(&TaskImageInputs {
            environment_digest: &request.environment.digest,
            snapshot: &request.materialized.snapshot,
            template_id: request.template_id,
            template_version: request.template_version,
            configuration_digest: request.configuration_digest,
            created: request.created,
        });
        let image = ImageRef::new(format!("{}:{}", request.repository, identity.tag()))
            .map_err(TaskImageError::InvalidRequest)?;
        let context = tempfile::tempdir().map_err(context_error("create task build context"))?;
        write_context(context.path(), &request, &identity)?;

        let environment = immutable_environment_ref(request.environment)?;
        let plan = TemplatePlan {
            template_id: request.template_id.to_owned(),
            template_version: request.template_version.to_owned(),
            base_image: environment,
            platform: request.platform,
            target_platforms: vec![request.platform],
            build_context: PathBuf::from("."),
            parameters: Default::default(),
            stages: Vec::new(),
        };
        let mut build_args = std::collections::BTreeMap::new();
        for (name, value) in labels(&request, &identity) {
            build_args.insert(name.to_owned(), value);
        }
        let result = self
            .buildkit
            .build(
                BuildRequest {
                    plan: &plan,
                    catalog_root: context.path(),
                    image,
                    options: BuildOptions {
                        progress: request.options.progress,
                        output: ImageOutput::Load,
                        cache: request.options.cache,
                        builder: request.options.builder,
                        build_args,
                        ..BuildOptions::default()
                    },
                },
                cancellation,
            )
            .map_err(TaskImageError::Build)?;
        Ok(BuiltTaskImage {
            image: result,
            identity,
        })
    }
}

fn validate_request(request: &TaskImageRequest<'_>) -> Result<(), TaskImageError> {
    if request.template_id.is_empty() || request.template_version.is_empty() {
        return Err(TaskImageError::InvalidRequest(
            "template ID and version must be non-empty".to_owned(),
        ));
    }
    if request.created.is_empty() || request.created.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(TaskImageError::InvalidRequest(
            "created timestamp must be a non-empty single-line OCI value".to_owned(),
        ));
    }
    validate_repository(request.repository)?;
    let metadata = fs::symlink_metadata(request.materialized.path())
        .map_err(context_error("inspect source snapshot"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TaskImageError::InvalidRequest(
            "snapshot root must be a real directory".to_owned(),
        ));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<(), TaskImageError> {
    let leaf = repository.rsplit('/').next().unwrap_or(repository);
    if repository.is_empty()
        || repository.contains('@')
        || leaf.contains(':')
        || repository
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        Err(TaskImageError::InvalidRequest(
            "task image repository must be non-empty and contain neither a tag nor digest"
                .to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn immutable_environment_ref(environment: &BuiltImage) -> Result<String, TaskImageError> {
    if environment.image.as_str().contains('@') {
        return Err(TaskImageError::InvalidRequest(
            "environment image reference must not already contain a digest".to_owned(),
        ));
    }
    Ok(format!("{}@{}", environment.image, environment.digest))
}

fn write_context(
    root: &Path,
    request: &TaskImageRequest<'_>,
    identity: &TaskImageIdentity,
) -> Result<(), TaskImageError> {
    fs::write(root.join("Dockerfile"), dockerfile())
        .map_err(context_error("write task Dockerfile"))?;
    fs::write(root.join(".dockerignore"), DOCKERIGNORE)
        .map_err(context_error("write task .dockerignore"))?;
    let destination = root.join("source");
    fs::create_dir(&destination).map_err(context_error("create source context"))?;
    validate_context_paths(request.materialized.path())?;
    let copied = request
        .materialized
        .copy_verified_to(&destination)
        .map_err(TaskImageError::Snapshot)?;
    if copied != request.materialized.snapshot.file_count {
        return Err(TaskImageError::InvalidRequest(format!(
            "snapshot file count is {}, but materialized tree contains {copied}",
            request.materialized.snapshot.file_count
        )));
    }
    // Keep this referenced here so context construction and label construction cannot
    // accidentally diverge during later changes.
    debug_assert_eq!(identity.as_str().len(), 64);
    Ok(())
}

fn dockerfile() -> &'static str {
    r#"ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG TASK_CREATED
ARG TASK_SOURCE_COMMIT
ARG TASK_SOURCE_DIGEST
ARG TASK_TEMPLATE_ID
ARG TASK_TEMPLATE_VERSION
ARG TASK_CONFIG_DIGEST
ARG TASK_ENVIRONMENT_DIGEST
ARG TASK_IDENTITY
COPY --link source/ /workspace/
WORKDIR /workspace
LABEL org.opencontainers.image.created="${TASK_CREATED}" \
      org.opencontainers.image.revision="${TASK_SOURCE_COMMIT}" \
      org.opencontainers.image.version="${TASK_TEMPLATE_VERSION}" \
      io.repo-sandbox.source.commit="${TASK_SOURCE_COMMIT}" \
      io.repo-sandbox.source.digest="${TASK_SOURCE_DIGEST}" \
      io.repo-sandbox.template.id="${TASK_TEMPLATE_ID}" \
      io.repo-sandbox.template.version="${TASK_TEMPLATE_VERSION}" \
      io.repo-sandbox.config.digest="${TASK_CONFIG_DIGEST}" \
      io.repo-sandbox.environment.digest="${TASK_ENVIRONMENT_DIGEST}" \
      io.repo-sandbox.task.identity="${TASK_IDENTITY}"
"#
}

fn labels(
    request: &TaskImageRequest<'_>,
    identity: &TaskImageIdentity,
) -> Vec<(&'static str, String)> {
    vec![
        ("TASK_CREATED", request.created.to_owned()),
        (
            "TASK_SOURCE_COMMIT",
            source_commit(&request.materialized.snapshot)
                .unwrap_or("local")
                .to_owned(),
        ),
        (
            "TASK_SOURCE_DIGEST",
            format!("sha256:{}", request.materialized.snapshot.id),
        ),
        ("TASK_TEMPLATE_ID", request.template_id.to_owned()),
        ("TASK_TEMPLATE_VERSION", request.template_version.to_owned()),
        (
            "TASK_CONFIG_DIGEST",
            request.configuration_digest.oci_value(),
        ),
        (
            "TASK_ENVIRONMENT_DIGEST",
            request.environment.digest.to_string(),
        ),
        ("TASK_IDENTITY", identity.oci_value()),
    ]
}

fn validate_context_paths(source: &Path) -> Result<(), TaskImageError> {
    for entry in fs::read_dir(source).map_err(context_error("read source snapshot"))? {
        let entry = entry.map_err(context_error("read source snapshot entry"))?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if forbidden_name(&name_text) {
            return Err(TaskImageError::InvalidRequest(format!(
                "snapshot contains forbidden metadata or credential path `{name_text}`"
            )));
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(context_error("inspect source snapshot entry"))?;
        if metadata.file_type().is_symlink() {
            return Err(TaskImageError::InvalidRequest(
                "snapshot context cannot contain symbolic links".to_owned(),
            ));
        }
        if metadata.is_dir() {
            validate_context_paths(&entry.path())?;
        } else if metadata.is_file() {
        } else {
            return Err(TaskImageError::InvalidRequest(
                "snapshot context cannot contain special files".to_owned(),
            ));
        }
    }
    Ok(())
}

fn forbidden_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | ".env"
            | ".netrc"
            | "_netrc"
            | "id_rsa"
            | "id_ed25519"
            | "credentials"
            | ".docker"
            | ".git-credentials"
            | ".npmrc"
            | ".pypirc"
            | ".ssh"
            | ".aws"
    ) || name.to_ascii_lowercase().starts_with(".env.")
}

fn context_error(operation: &'static str) -> impl FnOnce(io::Error) -> TaskImageError {
    move |source| TaskImageError::Context { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buildkit::{
        NeverCancelled, ProcessInvocation, ProcessOutput, SystemProcessExecutor,
    };
    use crate::snapshot::GitSnapshotter;
    use repo_sandbox_core::build::{ImageDigest, PlatformDigest};
    use repo_sandbox_core::snapshot::{
        CleanupPolicy, GitAuthentication, SnapshotOptions, SourceSpec,
    };
    use repo_sandbox_core::task_image::TASK_WORKDIR;
    use std::process::Command;
    use std::sync::Mutex;

    const RESULT_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct InspectingExecutor {
        invocations: Mutex<Vec<ProcessInvocation>>,
    }

    impl InspectingExecutor {
        fn new() -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProcessExecutor for &InspectingExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> io::Result<ProcessOutput> {
            self.invocations.lock().unwrap().push(invocation.clone());
            if invocation.args.get(1).map(String::as_str) == Some("build") {
                let context = PathBuf::from(invocation.args.last().unwrap());
                let example = context.join("source/src/lib.rs");
                if example.exists() {
                    assert_eq!(
                        fs::read_to_string(example)?,
                        "pub fn answer() -> u8 { 42 }\n"
                    );
                }
                let ignore = fs::read_to_string(context.join(".dockerignore"))?;
                assert!(ignore.contains("source/**/.git"));
                assert!(ignore.contains("source/**/.env.*"));
                let dockerfile = fs::read_to_string(context.join("Dockerfile"))?;
                assert!(dockerfile.contains("COPY --link source/ /workspace/"));
                assert!(!dockerfile.contains("ENTRYPOINT"));
                let metadata = PathBuf::from(value_after(&invocation.args, "--metadata-file"));
                fs::write(
                    metadata,
                    format!(r#"{{"containerimage.digest":"{RESULT_DIGEST}"}}"#),
                )?;
            }
            Ok(ProcessOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let index = args.iter().position(|value| value == flag).unwrap();
        &args[index + 1]
    }

    fn materialize(files: &[(&str, &str)]) -> (tempfile::TempDir, MaterializedSnapshot) {
        let repository = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repository.path())
            .status()
            .unwrap();
        assert!(status.success());
        for (path, contents) in files {
            let path = repository.path().join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        let materialized = GitSnapshotter::default()
            .with_authentication(GitAuthentication::None)
            .create(
                &SourceSpec::LocalDirectory(repository.path().to_owned()),
                SnapshotOptions {
                    recurse_submodules: false,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .unwrap();
        (repository, materialized)
    }

    fn environment() -> BuiltImage {
        let digest = ImageDigest::new(format!("sha256:{}", "e".repeat(64))).unwrap();
        BuiltImage {
            image: ImageRef::new("repo-sandbox/environment:stable").unwrap(),
            digest: digest.clone(),
            platform_digests: vec![PlatformDigest {
                platform: Platform::LinuxAmd64,
                digest,
            }],
        }
    }

    fn config() -> ConfigurationDigest {
        ConfigurationDigest::parse("c".repeat(64)).unwrap()
    }

    fn request<'a>(
        environment: &'a BuiltImage,
        materialized: &'a MaterializedSnapshot,
        config: &'a ConfigurationDigest,
    ) -> TaskImageRequest<'a> {
        TaskImageRequest {
            environment,
            materialized,
            template_id: "rust-bazel",
            template_version: "1.0.0",
            platform: Platform::LinuxAmd64,
            configuration_digest: config,
            created: "2026-09-01T00:00:00Z",
            repository: "repo-sandbox/task",
            options: TaskImageOptions::default(),
        }
    }

    #[test]
    fn creates_minimal_context_and_content_addressed_tag() {
        let (_repository, materialized) =
            materialize(&[("src/lib.rs", "pub fn answer() -> u8 { 42 }\n")]);
        let environment = environment();
        let config = config();
        let executor = InspectingExecutor::new();
        let first = TaskImageBuilder::new(&executor)
            .build(
                request(&environment, &materialized, &config),
                &NeverCancelled,
            )
            .unwrap();
        let second = TaskImageBuilder::new(&executor)
            .build(
                request(&environment, &materialized, &config),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.image.image, second.image.image);
        assert!(first.image.image.as_str().contains(":sha256-"));

        let invocations = executor.invocations.lock().unwrap();
        let args = &invocations[0].args;
        assert!(args.windows(2).any(|pair| pair[0] == "--build-arg"
            && pair[1] == format!("TASK_SOURCE_DIGEST=sha256:{}", materialized.snapshot.id)));
        assert!(args.windows(2).any(|pair| pair == ["--build-arg", "TASK_CONFIG_DIGEST=sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"]));
        assert!(args.windows(2).any(|pair| pair == ["--build-arg", "BASE_IMAGE=repo-sandbox/environment:stable@sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"]));
    }

    #[test]
    fn rejects_git_and_credential_files_before_docker_runs() {
        for forbidden in [
            ".git",
            ".env",
            ".env.production",
            ".netrc",
            "id_rsa",
            "credentials",
            ".docker",
            ".ssh",
            ".aws",
            ".git-credentials",
            ".npmrc",
        ] {
            let (_repository, materialized) = materialize(&[("safe.txt", "safe")]);
            let path = materialized.path().join(forbidden);
            if forbidden.starts_with('.') && matches!(forbidden, ".docker" | ".ssh" | ".aws") {
                fs::create_dir(&path).unwrap();
                fs::write(path.join("credentials"), "injected secret").unwrap();
            } else {
                fs::write(path, "injected secret").unwrap();
            }
            let environment = environment();
            let config = config();
            let executor = InspectingExecutor::new();
            let error = TaskImageBuilder::new(&executor)
                .build(
                    request(&environment, &materialized, &config),
                    &NeverCancelled,
                )
                .unwrap_err();
            assert!(error.to_string().contains("forbidden"));
            assert!(executor.invocations.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn tagged_repository_is_rejected() {
        let (_repository, materialized) = materialize(&[("source.rs", "safe")]);
        let environment = environment();
        let config = config();
        let executor = InspectingExecutor::new();
        let mut tagged = request(&environment, &materialized, &config);
        tagged.repository = "repo-sandbox/task:latest";
        assert!(
            TaskImageBuilder::new(&executor)
                .build(tagged, &NeverCancelled)
                .is_err()
        );
    }

    #[test]
    fn same_file_count_content_and_path_tampering_fail_before_docker() {
        for mutate in ["content", "path"] {
            let (_repository, materialized) = materialize(&[("source.rs", "safe")]);
            if mutate == "content" {
                fs::write(materialized.path().join("source.rs"), "evil").unwrap();
            } else {
                fs::rename(
                    materialized.path().join("source.rs"),
                    materialized.path().join("replacement.rs"),
                )
                .unwrap();
            }
            let environment = environment();
            let config = config();
            let executor = InspectingExecutor::new();
            let error = TaskImageBuilder::new(&executor)
                .build(
                    request(&environment, &materialized, &config),
                    &NeverCancelled,
                )
                .unwrap_err();
            assert!(error.to_string().contains("snapshot"));
            assert!(executor.invocations.lock().unwrap().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn mode_tampering_fails_before_docker() {
        use std::os::unix::fs::PermissionsExt;
        let (_repository, materialized) = materialize(&[("source.sh", "echo safe\n")]);
        fs::set_permissions(
            materialized.path().join("source.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let environment = environment();
        let config = config();
        let executor = InspectingExecutor::new();
        let error = TaskImageBuilder::new(&executor)
            .build(
                request(&environment, &materialized, &config),
                &NeverCancelled,
            )
            .unwrap_err();
        assert!(error.to_string().contains("mode changed"));
        assert!(executor.invocations.lock().unwrap().is_empty());
    }

    #[test]
    fn different_created_labels_cannot_reuse_a_tag() {
        let (_repository, materialized) = materialize(&[("source.rs", "safe")]);
        let environment = environment();
        let config = config();
        let executor = InspectingExecutor::new();
        let first = TaskImageBuilder::new(&executor)
            .build(
                request(&environment, &materialized, &config),
                &NeverCancelled,
            )
            .unwrap();
        let mut changed = request(&environment, &materialized, &config);
        changed.created = "2026-09-01T00:00:01Z";
        let second = TaskImageBuilder::new(&executor)
            .build(changed, &NeverCancelled)
            .unwrap();
        assert_ne!(first.identity, second.identity);
        assert_ne!(first.image.image, second.image.image);
    }

    /// Optional end-to-end check. It validates labels, history, exported files, and workdir.
    #[test]
    #[ignore = "requires an accessible Linux Docker daemon, buildx, and busybox:1.36"]
    fn docker_task_image_contains_only_snapshot_source() {
        let (root, materialized) = materialize(&[("answer.txt", "42\n")]);
        let run = |args: Vec<String>| {
            SystemProcessExecutor
                .execute(
                    &ProcessInvocation {
                        program: "docker".to_owned(),
                        args,
                        current_dir: None,
                    },
                    &NeverCancelled,
                )
                .unwrap()
        };
        let pull = run(vec!["pull".into(), "busybox:1.36".into()]);
        assert_eq!(pull.exit_code, Some(0), "{}", pull.stderr);
        let parent = run(vec![
            "image".into(),
            "inspect".into(),
            "busybox:1.36".into(),
            "--format".into(),
            "{{index .RepoDigests 0}}".into(),
        ]);
        let (_, parent_digest) = parent.stdout.trim().split_once('@').unwrap();
        let environment = BuiltImage {
            image: ImageRef::new("busybox:1.36").unwrap(),
            digest: ImageDigest::new(parent_digest).unwrap(),
            platform_digests: Vec::new(),
        };
        let config = config();
        let built = TaskImageBuilder::new(SystemProcessExecutor)
            .build(
                request(&environment, &materialized, &config),
                &NeverCancelled,
            )
            .unwrap();
        let image = built.image.image.to_string();
        let inspect = run(vec![
            "image".into(),
            "inspect".into(),
            image.clone(),
            "--format".into(),
            "{{json .Config.Labels}} {{.Config.WorkingDir}}".into(),
        ]);
        assert_eq!(inspect.exit_code, Some(0), "{}", inspect.stderr);
        assert!(
            inspect
                .stdout
                .contains(format!("sha256:{}", materialized.snapshot.id).as_str())
        );
        assert!(inspect.stdout.contains(TASK_WORKDIR));
        let history = run(vec!["history".into(), "--no-trunc".into(), image.clone()]);
        assert!(!history.stdout.to_ascii_lowercase().contains(".git"));
        assert!(!history.stdout.to_ascii_lowercase().contains("credential"));
        assert!(!history.stdout.contains("injected secret"));
        let content = run(vec![
            "run".into(),
            "--rm".into(),
            image.clone(),
            "cat".into(),
            "/workspace/answer.txt".into(),
        ]);
        assert_eq!(content.stdout, "42\n");
        let container = run(vec!["create".into(), image.clone()]);
        let container_id = container.stdout.trim().to_owned();
        let archive = root.path().join("filesystem.tar");
        let exported = run(vec![
            "export".into(),
            "--output".into(),
            archive.to_string_lossy().into_owned(),
            container_id.clone(),
        ]);
        assert_eq!(exported.exit_code, Some(0), "{}", exported.stderr);
        let listing = SystemProcessExecutor
            .execute(
                &ProcessInvocation {
                    program: "tar".to_owned(),
                    args: vec!["-tf".into(), archive.to_string_lossy().into_owned()],
                    current_dir: None,
                },
                &NeverCancelled,
            )
            .unwrap();
        let paths = listing.stdout.to_ascii_lowercase();
        assert!(
            !paths
                .lines()
                .any(|path| path.contains("/.git/") || path.ends_with("/.git"))
        );
        assert!(!paths.lines().any(|path| path.contains("/.env")));
        let _ = run(vec!["rm".into(), "--force".into(), container_id]);
        let _ = run(vec!["image".into(), "rm".into(), "--force".into(), image]);
    }
}
