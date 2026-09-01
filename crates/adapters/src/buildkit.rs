//! Docker Buildx/BuildKit adapter for central environment templates.

use repo_sandbox_core::build::{BuiltImage, ImageDigest, ImageRef};
use repo_sandbox_core::template::TemplatePlan;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// A process invocation represented as an argv vector. No command shell is involved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub interrupted: bool,
}

pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Injectable process boundary used by both production and deterministic tests.
pub trait ProcessExecutor {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ProcessOutput>;
}

impl<T: ProcessExecutor + ?Sized> ProcessExecutor for &T {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ProcessOutput> {
        (**self).execute(invocation, cancellation)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcessExecutor;

impl ProcessExecutor for SystemProcessExecutor {
    fn execute(
        &self,
        invocation: &ProcessInvocation,
        cancellation: &dyn Cancellation,
    ) -> io::Result<ProcessOutput> {
        let mut child = Command::new(&invocation.program)
            .args(&invocation.args)
            .current_dir(invocation.current_dir.as_deref().unwrap_or(Path::new(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout is available");
        let stderr = child.stderr.take().expect("piped stderr is available");
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));
        let (status, interrupted) = loop {
            if cancellation.is_cancelled() {
                child.kill()?;
                break (child.wait()?, true);
            }
            if let Some(status) = child.try_wait()? {
                break (status, false);
            }
            thread::sleep(Duration::from_millis(25));
        };
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout: join_reader(stdout_reader)?,
            stderr: join_reader(stderr_reader)?,
            interrupted,
        })
    }
}

fn read_stream(mut stream: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<String> {
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("process output reader panicked"))??;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Progress {
    #[default]
    Auto,
    Plain,
    Tty,
    RawJson,
}

impl Progress {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plain => "plain",
            Self::Tty => "tty",
            Self::RawJson => "rawjson",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ImageOutput {
    #[default]
    Load,
    Push,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyConfig {
    pub http: Option<String>,
    pub https: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CacheConfig {
    /// Buildx cache descriptors such as `type=local,src=.cache/buildkit`.
    pub imports: Vec<String>,
    /// Buildx cache descriptors such as `type=local,dest=.cache/buildkit,mode=max`.
    pub exports: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Builder {
    /// Use the caller's selected builder, or the explicitly named existing builder.
    Existing(Option<String>),
    /// Create a docker-container builder owned by this build and remove it afterwards.
    Ephemeral { name: String },
}

impl Default for Builder {
    fn default() -> Self {
        Self::Existing(None)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildOptions {
    pub progress: Progress,
    pub output: ImageOutput,
    pub proxy: ProxyConfig,
    pub cache: CacheConfig,
    pub builder: Builder,
    /// Additional non-template build arguments. Reserved adapter arguments cannot be replaced.
    pub build_args: BTreeMap<String, String>,
}

pub struct BuildRequest<'a> {
    pub plan: &'a TemplatePlan,
    pub catalog_root: &'a Path,
    pub image: ImageRef,
    pub options: BuildOptions,
}

#[derive(Debug)]
pub enum BuildError {
    InvalidRequest(String),
    Process {
        operation: &'static str,
        source: io::Error,
    },
    Failed {
        operation: &'static str,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Interrupted {
        stdout: String,
        stderr: String,
    },
    Metadata(String),
    CleanupAfter {
        primary: Box<BuildError>,
        cleanup: Box<BuildError>,
    },
}

impl BuildError {
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Failed { exit_code, .. } => *exit_code,
            Self::CleanupAfter { primary, .. } => primary.exit_code(),
            _ => None,
        }
    }

    pub fn stderr(&self) -> Option<&str> {
        match self {
            Self::Failed { stderr, .. } | Self::Interrupted { stderr, .. } => Some(stderr),
            Self::CleanupAfter { primary, .. } => primary.stderr(),
            _ => None,
        }
    }
}

impl Display for BuildError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid build request: {message}"),
            Self::Process { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Failed {
                operation,
                exit_code,
                stderr,
                ..
            } => write!(
                formatter,
                "{operation} exited with {}: {}",
                exit_code.map_or_else(|| "no exit code".to_owned(), |code| code.to_string()),
                stderr.trim()
            ),
            Self::Interrupted { stderr, .. } => {
                write!(
                    formatter,
                    "BuildKit build was interrupted: {}",
                    stderr.trim()
                )
            }
            Self::Metadata(message) => write!(formatter, "invalid BuildKit metadata: {message}"),
            Self::CleanupAfter { primary, cleanup } => {
                write!(
                    formatter,
                    "{primary}; owned builder cleanup also failed: {cleanup}"
                )
            }
        }
    }
}

impl Error for BuildError {}

pub struct BuildKit<E> {
    executor: E,
}

impl<E> BuildKit<E> {
    pub const fn new(executor: E) -> Self {
        Self { executor }
    }
}

impl<E: ProcessExecutor> BuildKit<E> {
    pub fn build(
        &self,
        request: BuildRequest<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<BuiltImage, BuildError> {
        validate_options(&request.options)?;
        let ephemeral = match &request.options.builder {
            Builder::Ephemeral { name } => Some(name.as_str()),
            Builder::Existing(_) => None,
        };
        if let Some(name) = ephemeral {
            let create = ProcessInvocation {
                program: "docker".to_owned(),
                args: vec![
                    "buildx".to_owned(),
                    "create".to_owned(),
                    "--name".to_owned(),
                    name.to_owned(),
                    "--driver".to_owned(),
                    "docker-container".to_owned(),
                ],
                current_dir: None,
            };
            if let Err(primary) = self.run("create Buildx builder", &create, cancellation) {
                return match self.remove_builder(name) {
                    Ok(()) => Err(primary),
                    Err(cleanup) => Err(BuildError::CleanupAfter {
                        primary: Box::new(primary),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
        }

        let primary = self.build_inner(&request, cancellation);
        let cleanup = ephemeral.map(|name| self.remove_builder(name));
        match (primary, cleanup) {
            (result, None) | (result, Some(Ok(()))) => result,
            (Ok(_), Some(Err(cleanup))) => Err(cleanup),
            (Err(primary), Some(Err(cleanup))) => Err(BuildError::CleanupAfter {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            }),
        }
    }

    fn build_inner(
        &self,
        request: &BuildRequest<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<BuiltImage, BuildError> {
        let metadata_dir = tempfile::tempdir().map_err(|source| BuildError::Process {
            operation: "create build metadata directory",
            source,
        })?;
        let metadata_path = metadata_dir.path().join("metadata.json");
        let invocation = build_invocation(request, &metadata_path)?;
        self.run("docker buildx build", &invocation, cancellation)?;
        let source = fs::read_to_string(&metadata_path).map_err(|error| {
            BuildError::Metadata(format!("{}: {error}", metadata_path.display()))
        })?;
        let digest = json_string_field(&source, "containerimage.digest")
            .ok_or_else(|| BuildError::Metadata("missing `containerimage.digest`".to_owned()))?;
        Ok(BuiltImage {
            image: request.image.clone(),
            digest: ImageDigest::new(digest).map_err(BuildError::Metadata)?,
        })
    }

    fn remove_builder(&self, name: &str) -> Result<(), BuildError> {
        let invocation = ProcessInvocation {
            program: "docker".to_owned(),
            args: vec![
                "buildx".to_owned(),
                "rm".to_owned(),
                "--force".to_owned(),
                name.to_owned(),
            ],
            current_dir: None,
        };
        self.run("remove owned Buildx builder", &invocation, &NeverCancelled)
    }

    fn run(
        &self,
        operation: &'static str,
        invocation: &ProcessInvocation,
        cancellation: &dyn Cancellation,
    ) -> Result<(), BuildError> {
        let output = self
            .executor
            .execute(invocation, cancellation)
            .map_err(|source| BuildError::Process { operation, source })?;
        if output.interrupted {
            return Err(BuildError::Interrupted {
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
        if output.exit_code != Some(0) {
            return Err(BuildError::Failed {
                operation,
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
        Ok(())
    }
}

fn validate_options(options: &BuildOptions) -> Result<(), BuildError> {
    if let Builder::Ephemeral { name } = &options.builder {
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !valid {
            return Err(BuildError::InvalidRequest(
                "ephemeral builder name must contain 1-64 ASCII letters, digits, `-`, or `_`"
                    .to_owned(),
            ));
        }
    }
    for (name, value) in &options.build_args {
        if name.trim().is_empty() || value.contains('\0') {
            return Err(BuildError::InvalidRequest(format!(
                "invalid build argument `{name}`"
            )));
        }
    }
    Ok(())
}

fn build_invocation(
    request: &BuildRequest<'_>,
    metadata_path: &Path,
) -> Result<ProcessInvocation, BuildError> {
    let context = request.catalog_root.join(&request.plan.build_context);
    let mut args = vec!["buildx".to_owned(), "build".to_owned()];
    if let Some(builder) = match &request.options.builder {
        Builder::Existing(name) => name.as_deref(),
        Builder::Ephemeral { name } => Some(name.as_str()),
    } {
        args.extend(["--builder".to_owned(), builder.to_owned()]);
    }
    args.extend([
        "--platform".to_owned(),
        request.plan.platform.to_string(),
        "--progress".to_owned(),
        request.options.progress.as_str().to_owned(),
        "--metadata-file".to_owned(),
        metadata_path.to_string_lossy().into_owned(),
        "--tag".to_owned(),
        request.image.to_string(),
    ]);
    args.push(
        match request.options.output {
            ImageOutput::Load => "--load",
            ImageOutput::Push => "--push",
        }
        .to_owned(),
    );

    let reserved = build_arguments(request.plan)?;
    let reserved_names = reserved.keys().cloned().collect::<BTreeSet<_>>();
    for (name, value) in reserved
        .into_iter()
        .chain(request.options.build_args.clone())
    {
        if reserved_names.contains(&name) && request.options.build_args.contains_key(&name) {
            return Err(BuildError::InvalidRequest(format!(
                "build argument `{name}` is reserved by the template adapter"
            )));
        }
        push_pair(&mut args, "--build-arg", format!("{name}={value}"));
    }
    for (name, value) in [
        ("HTTP_PROXY", request.options.proxy.http.as_ref()),
        ("HTTPS_PROXY", request.options.proxy.https.as_ref()),
        ("NO_PROXY", request.options.proxy.no_proxy.as_ref()),
    ] {
        if let Some(value) = value {
            push_pair(&mut args, "--build-arg", format!("{name}={value}"));
        }
    }
    for cache in &request.options.cache.imports {
        push_pair(&mut args, "--cache-from", cache.clone());
    }
    for cache in &request.options.cache.exports {
        push_pair(&mut args, "--cache-to", cache.clone());
    }
    args.push(context.to_string_lossy().into_owned());
    Ok(ProcessInvocation {
        program: "docker".to_owned(),
        args,
        current_dir: Some(request.catalog_root.to_owned()),
    })
}

fn push_pair(args: &mut Vec<String>, flag: &str, value: String) {
    args.push(flag.to_owned());
    args.push(value);
}

fn build_arguments(plan: &TemplatePlan) -> Result<BTreeMap<String, String>, BuildError> {
    let mut arguments = BTreeMap::from([
        ("BASE_IMAGE".to_owned(), plan.base_image.clone()),
        (
            "REPO_SANDBOX_TEMPLATE_ID".to_owned(),
            plan.template_id.clone(),
        ),
        (
            "REPO_SANDBOX_TEMPLATE_VERSION".to_owned(),
            plan.template_version.clone(),
        ),
        ("REPO_SANDBOX_PLAN_DIGEST".to_owned(), plan_digest(plan)),
    ]);
    for (name, value) in &plan.parameters {
        let name = parameter_argument_name(name)?;
        if arguments.insert(name.clone(), value.clone()).is_some() {
            return Err(BuildError::InvalidRequest(format!(
                "template parameters collide at build argument `{name}`"
            )));
        }
    }
    Ok(arguments)
}

fn parameter_argument_name(name: &str) -> Result<String, BuildError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(BuildError::InvalidRequest(format!(
            "template parameter `{name}` cannot be represented as a build argument"
        )));
    }
    Ok(name
        .bytes()
        .map(|byte| {
            if byte == b'-' {
                b'_'
            } else {
                byte.to_ascii_uppercase()
            }
        })
        .map(char::from)
        .collect())
}

/// Stable fingerprint used by the Dockerfile so plan changes invalidate image configuration.
pub fn plan_digest(plan: &TemplatePlan) -> String {
    let mut hasher = Sha256::new();
    for value in [
        plan.template_id.as_str(),
        plan.template_version.as_str(),
        plan.base_image.as_str(),
        plan.platform.as_str(),
        &plan.build_context.to_string_lossy(),
    ] {
        hash_field(&mut hasher, value);
    }
    for (name, value) in &plan.parameters {
        hash_field(&mut hasher, name);
        hash_field(&mut hasher, value);
    }
    for stage in &plan.stages {
        hash_field(&mut hasher, &stage.id);
        hash_field(&mut hasher, &stage.version);
        hash_field(&mut hasher, &stage.build_context.to_string_lossy());
        for dependency in &stage.depends_on {
            hash_field(&mut hasher, dependency);
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value.as_bytes());
}

fn json_string_field(source: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\"");
    let tail = source.get(source.find(&key)? + key.len()..)?;
    let (_, tail) = tail.split_once(':')?;
    let tail = tail.trim_start();
    let mut characters = tail.strip_prefix('"')?.chars();
    let mut value = String::new();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(value),
            '\\' => match characters.next()? {
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                '/' => value.push('/'),
                'b' => value.push('\u{8}'),
                'f' => value.push('\u{c}'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                _ => return None,
            },
            control if control.is_control() => return None,
            other => value.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_sandbox_core::config::Platform;
    use repo_sandbox_core::template::PlanStage;
    use std::sync::Mutex;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone, Copy)]
    enum BuildBehavior {
        Success,
        Failure,
        Interrupted,
    }

    struct FakeExecutor {
        behavior: BuildBehavior,
        invocations: Mutex<Vec<ProcessInvocation>>,
    }

    impl FakeExecutor {
        fn new(behavior: BuildBehavior) -> Self {
            Self {
                behavior,
                invocations: Mutex::new(Vec::new()),
            }
        }

        fn invocations(&self) -> Vec<ProcessInvocation> {
            self.invocations.lock().unwrap().clone()
        }
    }

    impl ProcessExecutor for FakeExecutor {
        fn execute(
            &self,
            invocation: &ProcessInvocation,
            _cancellation: &dyn Cancellation,
        ) -> io::Result<ProcessOutput> {
            self.invocations.lock().unwrap().push(invocation.clone());
            let is_build = invocation.args.get(1).is_some_and(|value| value == "build");
            if is_build {
                match self.behavior {
                    BuildBehavior::Success => {
                        let index = invocation
                            .args
                            .iter()
                            .position(|value| value == "--metadata-file")
                            .unwrap();
                        fs::write(
                            &invocation.args[index + 1],
                            format!(r#"{{"containerimage.digest":"{DIGEST}"}}"#),
                        )?;
                    }
                    BuildBehavior::Failure => {
                        return Ok(ProcessOutput {
                            exit_code: Some(42),
                            stdout: "#7 compiling".to_owned(),
                            stderr: "Dockerfile:17: RUN cargo build: exit code: 101".to_owned(),
                            interrupted: false,
                        });
                    }
                    BuildBehavior::Interrupted => {
                        return Ok(ProcessOutput {
                            exit_code: None,
                            stdout: "#5 building".to_owned(),
                            stderr: "canceled: context canceled".to_owned(),
                            interrupted: true,
                        });
                    }
                }
            }
            Ok(ProcessOutput {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                interrupted: false,
            })
        }
    }

    fn plan() -> TemplatePlan {
        TemplatePlan {
            template_id: "rust-bazel".to_owned(),
            template_version: "1.0.0".to_owned(),
            base_image: "docker.io/library/rust:1.97.0-bookworm".to_owned(),
            platform: Platform::LinuxAmd64,
            build_context: PathBuf::from("templates/rust-bazel/context"),
            parameters: BTreeMap::from([
                ("bazelisk_version".to_owned(), "1.27.0".to_owned()),
                ("rust_version".to_owned(), "1.97.0".to_owned()),
            ]),
            stages: vec![PlanStage {
                id: "base-tools".to_owned(),
                version: "1.0.0".to_owned(),
                build_context: PathBuf::from("templates/components/base-tools/context"),
                depends_on: Vec::new(),
            }],
        }
    }

    fn request(plan: &TemplatePlan, options: BuildOptions) -> BuildRequest<'_> {
        BuildRequest {
            plan,
            catalog_root: Path::new("catalog root with spaces"),
            image: ImageRef::new("registry.test/repo-sandbox/rust-bazel:test").unwrap(),
            options,
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let index = args.iter().position(|value| value == flag).unwrap();
        &args[index + 1]
    }

    #[test]
    fn cold_and_warm_builds_use_the_same_cache_contract_and_return_digest() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let adapter = BuildKit::new(&executor);
        let plan = plan();
        let options = BuildOptions {
            progress: Progress::Plain,
            cache: CacheConfig {
                imports: vec!["type=registry,ref=registry.test/cache:rust".to_owned()],
                exports: vec!["type=registry,ref=registry.test/cache:rust,mode=max".to_owned()],
            },
            ..BuildOptions::default()
        };
        let cold = adapter
            .build(request(&plan, options.clone()), &NeverCancelled)
            .unwrap();
        let warm = adapter
            .build(request(&plan, options), &NeverCancelled)
            .unwrap();
        assert_eq!(cold, warm);
        assert_eq!(cold.digest.as_str(), DIGEST);
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 2);
        for invocation in invocations {
            assert_eq!(
                value_after(&invocation.args, "--cache-from"),
                "type=registry,ref=registry.test/cache:rust"
            );
            assert_eq!(
                value_after(&invocation.args, "--cache-to"),
                "type=registry,ref=registry.test/cache:rust,mode=max"
            );
        }
    }

    #[test]
    fn template_and_parameter_changes_produce_distinct_cache_invalidation_digest() {
        let original = plan();
        let mut changed_parameter = original.clone();
        changed_parameter
            .parameters
            .insert("rust_version".to_owned(), "1.98.0".to_owned());
        changed_parameter.base_image = "docker.io/library/rust:1.98.0-bookworm".to_owned();
        let mut changed_template = original.clone();
        changed_template.template_version = "1.1.0".to_owned();
        assert_ne!(plan_digest(&original), plan_digest(&changed_parameter));
        assert_ne!(plan_digest(&original), plan_digest(&changed_template));

        let executor = FakeExecutor::new(BuildBehavior::Success);
        let adapter = BuildKit::new(&executor);
        adapter
            .build(request(&original, BuildOptions::default()), &NeverCancelled)
            .unwrap();
        adapter
            .build(
                request(&changed_parameter, BuildOptions::default()),
                &NeverCancelled,
            )
            .unwrap();
        let invocations = executor.invocations();
        let digest_arg = |invocation: &ProcessInvocation| {
            invocation
                .args
                .windows(2)
                .find(|pair| {
                    pair[0] == "--build-arg" && pair[1].starts_with("REPO_SANDBOX_PLAN_DIGEST=")
                })
                .unwrap()[1]
                .clone()
        };
        assert_ne!(digest_arg(&invocations[0]), digest_arg(&invocations[1]));
    }

    #[test]
    fn argv_keeps_platform_progress_proxy_and_build_args_structured() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let adapter = BuildKit::new(&executor);
        let plan = plan();
        adapter
            .build(
                request(
                    &plan,
                    BuildOptions {
                        progress: Progress::RawJson,
                        proxy: ProxyConfig {
                            http: Some("http://proxy.test:8080; echo not-a-shell".to_owned()),
                            https: None,
                            no_proxy: Some("localhost,.internal".to_owned()),
                        },
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap();
        let invocation = &executor.invocations()[0];
        assert_eq!(invocation.program, "docker");
        assert_eq!(value_after(&invocation.args, "--platform"), "linux/amd64");
        assert_eq!(value_after(&invocation.args, "--progress"), "rawjson");
        assert!(
            invocation
                .args
                .contains(&"HTTP_PROXY=http://proxy.test:8080; echo not-a-shell".to_owned())
        );
        assert_eq!(
            invocation.args.last().unwrap(),
            &Path::new("catalog root with spaces")
                .join("templates/rust-bazel/context")
                .to_string_lossy()
        );
    }

    #[test]
    fn buildkit_failure_preserves_reason_and_exit_code() {
        let executor = FakeExecutor::new(BuildBehavior::Failure);
        let plan = plan();
        let error = BuildKit::new(&executor)
            .build(request(&plan, BuildOptions::default()), &NeverCancelled)
            .unwrap_err();
        assert_eq!(error.exit_code(), Some(42));
        assert_eq!(
            error.stderr(),
            Some("Dockerfile:17: RUN cargo build: exit code: 101")
        );
        assert!(error.to_string().contains("exit code: 101"));
    }

    #[test]
    fn interruption_removes_only_the_builder_created_for_this_task() {
        let executor = FakeExecutor::new(BuildBehavior::Interrupted);
        let plan = plan();
        let error = BuildKit::new(&executor)
            .build(
                request(
                    &plan,
                    BuildOptions {
                        builder: Builder::Ephemeral {
                            name: "repo-sandbox-task-7".to_owned(),
                        },
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap_err();
        assert!(matches!(error, BuildError::Interrupted { .. }));
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 3);
        assert_eq!(
            invocations[2].args,
            ["buildx", "rm", "--force", "repo-sandbox-task-7"]
        );
        assert!(
            invocations
                .iter()
                .all(|invocation| !invocation.args.iter().any(|argument| argument == "prune"))
        );
        assert!(
            invocations
                .iter()
                .all(|invocation| !invocation.args.iter().any(|argument| argument == "system"))
        );
    }

    #[test]
    fn existing_builder_is_never_removed() {
        let executor = FakeExecutor::new(BuildBehavior::Failure);
        let plan = plan();
        let _ = BuildKit::new(&executor).build(
            request(
                &plan,
                BuildOptions {
                    builder: Builder::Existing(Some("shared-builder".to_owned())),
                    ..BuildOptions::default()
                },
            ),
            &NeverCancelled,
        );
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            value_after(&invocations[0].args, "--builder"),
            "shared-builder"
        );
    }

    /// Optional host integration check; unit and CI suites never require Docker.
    #[test]
    #[ignore = "requires an accessible Docker daemon and buildx"]
    fn docker_buildx_cold_and_warm_smoke() {
        let catalog = tempfile::tempdir().unwrap();
        let context = catalog.path().join("context");
        fs::create_dir(&context).unwrap();
        fs::write(
            context.join("Dockerfile"),
            r#"ARG BASE_IMAGE=scratch
FROM ${BASE_IMAGE}
ARG REPO_SANDBOX_TEMPLATE_ID
ARG REPO_SANDBOX_TEMPLATE_VERSION
ARG REPO_SANDBOX_PLAN_DIGEST
LABEL org.opencontainers.image.title="${REPO_SANDBOX_TEMPLATE_ID}" \
      org.opencontainers.image.version="${REPO_SANDBOX_TEMPLATE_VERSION}" \
      io.repo-sandbox.plan-digest="${REPO_SANDBOX_PLAN_DIGEST}"
"#,
        )
        .unwrap();
        let mut plan = plan();
        plan.base_image = "scratch".to_owned();
        plan.build_context = PathBuf::from("context");
        plan.stages.clear();
        let image =
            ImageRef::new(format!("repo-sandbox-issue8-smoke:{}", std::process::id())).unwrap();
        let adapter = BuildKit::new(SystemProcessExecutor);
        for _ in 0..2 {
            let result = adapter
                .build(
                    BuildRequest {
                        plan: &plan,
                        catalog_root: catalog.path(),
                        image: image.clone(),
                        options: BuildOptions {
                            progress: Progress::Plain,
                            ..BuildOptions::default()
                        },
                    },
                    &NeverCancelled,
                )
                .unwrap();
            assert_eq!(result.image, image);
        }
        let cleanup = ProcessInvocation {
            program: "docker".to_owned(),
            args: vec![
                "image".to_owned(),
                "rm".to_owned(),
                "--force".to_owned(),
                image.to_string(),
            ],
            current_dir: None,
        };
        let output = SystemProcessExecutor
            .execute(&cleanup, &NeverCancelled)
            .unwrap();
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
    }
}
