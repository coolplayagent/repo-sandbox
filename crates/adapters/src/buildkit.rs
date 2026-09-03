//! Docker Buildx/BuildKit adapter for central environment templates.

use crate::snapshot::{ProcessTree, configure_process_tree};
use repo_sandbox_core::build::{BuiltImage, ImageDigest, ImageRef, PlatformDigest};
use repo_sandbox_core::config::Platform;
use repo_sandbox_core::template::TemplatePlan;
use serde_yaml::Value;
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
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(invocation.current_dir.as_deref().unwrap_or(Path::new(".")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn()?;
        let process_tree = ProcessTree::attach(&mut child).inspect_err(|_error| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
        let stdout = child.stdout.take().expect("piped stdout is available");
        let stderr = child.stderr.take().expect("piped stderr is available");
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));
        let (status, interrupted) = loop {
            if cancellation.is_cancelled() {
                process_tree.terminate();
                break (child.wait()?, true);
            }
            if let Some(status) = child.try_wait()? {
                break (status, false);
            }
            thread::sleep(Duration::from_millis(25));
        };
        process_tree.terminate();
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ImageOutput {
    #[default]
    Load,
    Push,
    /// Export an unpacked OCI image layout without requiring a registry.
    OciDirectory(PathBuf),
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
    /// Empty selects the plan's platform. More than one platform creates an OCI image index.
    pub platforms: Vec<Platform>,
    /// Additional non-template build arguments. Reserved adapter arguments cannot be replaced.
    pub build_args: BTreeMap<String, String>,
    /// Named BuildKit contexts. Values are explicit immutable context descriptors.
    pub named_contexts: BTreeMap<String, String>,
}

pub struct BuildRequest<'a> {
    pub plan: &'a TemplatePlan,
    pub catalog_root: &'a Path,
    pub image: ImageRef,
    pub options: BuildOptions,
    target: ExportTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportTarget {
    Environment,
    Task,
}

impl ExportTarget {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Task => "task",
        }
    }
}

impl<'a> BuildRequest<'a> {
    /// Construct a central-template build. The exported target is fixed so a
    /// caller cannot select an assembly stage or leak its cache/secrets.
    pub fn environment(
        plan: &'a TemplatePlan,
        catalog_root: &'a Path,
        image: ImageRef,
        options: BuildOptions,
    ) -> Self {
        Self {
            plan,
            catalog_root,
            image,
            options,
            target: ExportTarget::Environment,
        }
    }

    pub(crate) fn task(
        plan: &'a TemplatePlan,
        catalog_root: &'a Path,
        image: ImageRef,
        options: BuildOptions,
    ) -> Self {
        Self {
            plan,
            catalog_root,
            image,
            options,
            target: ExportTarget::Task,
        }
    }
}

#[derive(Debug)]
pub enum BuildError {
    InvalidRequest(String),
    Capability(String),
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
            Self::Capability(message) => {
                write!(formatter, "platform capability unavailable: {message}")
            }
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
    native_platform: Option<Platform>,
}

impl<E> BuildKit<E> {
    pub const fn new(executor: E) -> Self {
        Self {
            executor,
            native_platform: None,
        }
    }

    /// Override host architecture discovery for deterministic orchestration tests.
    pub const fn with_native_platform(mut self, platform: Platform) -> Self {
        self.native_platform = Some(platform);
        self
    }
}

impl<E: ProcessExecutor> BuildKit<E> {
    pub fn build(
        &self,
        request: BuildRequest<'_>,
        cancellation: &dyn Cancellation,
    ) -> Result<BuiltImage, BuildError> {
        let platforms = validate_request(&request)?;
        let ephemeral = match &request.options.builder {
            Builder::Ephemeral { name } => Some(name.as_str()),
            Builder::Existing(_) => None,
        };
        if let Some(name) = ephemeral {
            self.ensure_builder_name_is_available(name)?;
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
            // Ownership starts only after buildx confirms successful creation. A failed
            // create can be an already-existing builder race, so deleting by name here
            // could remove a resource owned by another task.
            self.run("create Buildx builder", &create, cancellation)?;
        }

        let primary = self
            .ensure_platform_capabilities(&request.options.builder, &platforms, cancellation)
            .and_then(|()| self.build_inner(&request, &platforms, cancellation));
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
        platforms: &[Platform],
        cancellation: &dyn Cancellation,
    ) -> Result<BuiltImage, BuildError> {
        let metadata_dir = tempfile::tempdir().map_err(|source| BuildError::Process {
            operation: "create build metadata directory",
            source,
        })?;
        let metadata_path = metadata_dir.path().join("metadata.json");
        let invocation = build_invocation(request, platforms, &metadata_path)?;
        self.run("docker buildx build", &invocation, cancellation)?;
        let source = fs::read_to_string(&metadata_path).map_err(|error| {
            BuildError::Metadata(format!("{}: {error}", metadata_path.display()))
        })?;
        let digest = json_string_field(&source, "containerimage.digest")
            .ok_or_else(|| BuildError::Metadata("missing `containerimage.digest`".to_owned()))?;
        let digest = ImageDigest::new(digest).map_err(BuildError::Metadata)?;
        let platform_digests = if platforms.len() == 1 {
            vec![PlatformDigest {
                platform: platforms[0],
                digest: digest.clone(),
            }]
        } else {
            self.inspect_platform_digests(request, platforms, cancellation)?
        };
        Ok(BuiltImage {
            image: request.image.clone(),
            digest,
            platform_digests,
        })
    }

    fn ensure_platform_capabilities(
        &self,
        builder: &Builder,
        platforms: &[Platform],
        cancellation: &dyn Cancellation,
    ) -> Result<(), BuildError> {
        let native = self
            .native_platform
            .or_else(native_platform)
            .ok_or_else(|| {
                BuildError::Capability(format!(
                    "host architecture `{}` is unsupported; expected amd64 or arm64",
                    std::env::consts::ARCH
                ))
            })?;
        let cross = platforms
            .iter()
            .copied()
            .filter(|platform| *platform != native)
            .collect::<Vec<_>>();
        if cross.is_empty() {
            return Ok(());
        }

        let mut args = vec!["buildx".to_owned(), "inspect".to_owned()];
        if let Some(name) = match builder {
            Builder::Existing(name) => name.as_deref(),
            Builder::Ephemeral { name } => Some(name.as_str()),
        } {
            args.push(name.to_owned());
        }
        args.push("--bootstrap".to_owned());
        let invocation = ProcessInvocation {
            program: "docker".to_owned(),
            args,
            current_dir: None,
        };
        let output = self.output(
            "inspect Buildx cross-platform capability",
            &invocation,
            cancellation,
        )?;
        let missing = cross
            .into_iter()
            .filter(|platform| !builder_advertises_platform(&output.stdout, *platform))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(BuildError::Capability(format!(
                "builder does not advertise {}; install QEMU/binfmt or attach a native node, then verify `docker buildx inspect --bootstrap`",
                join_platforms(&missing)
            )))
        }
    }

    fn inspect_platform_digests(
        &self,
        request: &BuildRequest<'_>,
        platforms: &[Platform],
        cancellation: &dyn Cancellation,
    ) -> Result<Vec<PlatformDigest>, BuildError> {
        let manifest = match &request.options.output {
            ImageOutput::Push => {
                let invocation = ProcessInvocation {
                    program: "docker".to_owned(),
                    args: vec![
                        "buildx".to_owned(),
                        "imagetools".to_owned(),
                        "inspect".to_owned(),
                        "--raw".to_owned(),
                        request.image.to_string(),
                    ],
                    current_dir: None,
                };
                self.output(
                    "inspect pushed multi-platform manifest",
                    &invocation,
                    cancellation,
                )?
                .stdout
            }
            ImageOutput::OciDirectory(path) => {
                return parse_oci_platform_digests(path, platforms);
            }
            ImageOutput::Load => unreachable!("multi-platform load is rejected before execution"),
        };
        parse_platform_digests(&manifest, platforms)
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

    fn ensure_builder_name_is_available(&self, name: &str) -> Result<(), BuildError> {
        let invocation = ProcessInvocation {
            program: "docker".to_owned(),
            args: vec!["buildx".to_owned(), "inspect".to_owned(), name.to_owned()],
            current_dir: None,
        };
        let output = self
            .executor
            .execute(&invocation, &NeverCancelled)
            .map_err(|source| BuildError::Process {
                operation: "inspect requested Buildx builder name",
                source,
            })?;
        if output.interrupted {
            return Err(BuildError::Interrupted {
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }
        if output.exit_code == Some(0) {
            return Err(BuildError::InvalidRequest(format!(
                "ephemeral builder `{name}` already exists and is not owned by this task"
            )));
        }
        if builder_is_missing(&output.stderr) {
            Ok(())
        } else {
            Err(BuildError::Failed {
                operation: "inspect requested Buildx builder name",
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
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
        if !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
        Ok(())
    }

    fn output(
        &self,
        operation: &'static str,
        invocation: &ProcessInvocation,
        cancellation: &dyn Cancellation,
    ) -> Result<ProcessOutput, BuildError> {
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
        Ok(output)
    }
}

fn validate_request(request: &BuildRequest<'_>) -> Result<Vec<Platform>, BuildError> {
    let options = &request.options;
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
    for (name, value) in &options.named_contexts {
        if name.trim().is_empty()
            || name
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == b'=')
            || value.trim().is_empty()
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(BuildError::InvalidRequest(format!(
                "invalid named build context `{name}`"
            )));
        }
    }
    let platforms = if options.platforms.is_empty() {
        vec![request.plan.platform]
    } else {
        options.platforms.clone()
    };
    let unique = platforms.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != platforms.len() {
        return Err(BuildError::InvalidRequest(
            "target platforms must not contain duplicates".to_owned(),
        ));
    }
    let unsupported = platforms
        .iter()
        .copied()
        .filter(|platform| !request.plan.target_platforms.contains(platform))
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        return Err(BuildError::InvalidRequest(format!(
            "template plan does not support requested platform(s): {}",
            join_platforms(&unsupported)
        )));
    }
    if platforms.len() > 1 && matches!(options.output, ImageOutput::Load) {
        return Err(BuildError::InvalidRequest(
            "multi-platform images cannot use `--load`; select push or an OCI directory output"
                .to_owned(),
        ));
    }
    if let ImageOutput::OciDirectory(path) = &options.output {
        let path = path.to_string_lossy();
        if path.is_empty()
            || path
                .bytes()
                .any(|byte| matches!(byte, b',' | b'\n' | b'\r'))
        {
            return Err(BuildError::InvalidRequest(
                "OCI output directory must be non-empty and contain no comma or newline".to_owned(),
            ));
        }
    }
    Ok(platforms)
}

fn build_invocation(
    request: &BuildRequest<'_>,
    platforms: &[Platform],
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
        join_platforms(platforms),
        "--progress".to_owned(),
        request.options.progress.as_str().to_owned(),
        "--provenance".to_owned(),
        "false".to_owned(),
        "--metadata-file".to_owned(),
        metadata_path.to_string_lossy().into_owned(),
        "--tag".to_owned(),
        request.image.to_string(),
        "--target".to_owned(),
        request.target.as_str().to_owned(),
    ]);
    match &request.options.output {
        ImageOutput::Load => args.push("--load".to_owned()),
        ImageOutput::Push => args.push("--push".to_owned()),
        ImageOutput::OciDirectory(path) => {
            push_pair(
                &mut args,
                "--output",
                format!("type=oci,dest={},tar=false", path.to_string_lossy()),
            );
        }
    }

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
    for (name, value) in &request.options.named_contexts {
        push_pair(&mut args, "--build-context", format!("{name}={value}"));
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

fn builder_is_missing(stderr: &str) -> bool {
    let message = stderr.to_ascii_lowercase();
    message.contains("no builder")
        || message.contains("not found")
        || message.contains("does not exist")
}

fn native_platform() -> Option<Platform> {
    match std::env::consts::ARCH {
        "x86_64" => Some(Platform::LinuxAmd64),
        "aarch64" => Some(Platform::LinuxArm64),
        _ => None,
    }
}

fn join_platforms(platforms: &[Platform]) -> String {
    platforms
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn builder_advertises_platform(output: &str, platform: Platform) -> bool {
    output.lines().any(|line| {
        line.trim()
            .strip_prefix("Platforms:")
            .is_some_and(|values| {
                values.split(',').any(|value| {
                    value
                        .trim()
                        .trim_end_matches('*')
                        .split_once('/')
                        .is_some_and(|(os, architecture)| {
                            format!("{os}/{architecture}") == platform.as_str()
                        })
                })
            })
    })
}

fn parse_platform_digests(
    source: &str,
    requested: &[Platform],
) -> Result<Vec<PlatformDigest>, BuildError> {
    let value: Value = serde_yaml::from_str(source)
        .map_err(|error| BuildError::Metadata(format!("invalid image index JSON: {error}")))?;
    let mut found = BTreeMap::new();
    collect_platform_descriptors(&value, &mut found)?;
    finish_platform_digests(found, requested)
}

fn parse_oci_platform_digests(
    layout: &Path,
    requested: &[Platform],
) -> Result<Vec<PlatformDigest>, BuildError> {
    let source =
        fs::read_to_string(layout.join("index.json")).map_err(|source| BuildError::Process {
            operation: "read OCI image index",
            source,
        })?;
    let value: Value = serde_yaml::from_str(&source)
        .map_err(|error| BuildError::Metadata(format!("invalid OCI index JSON: {error}")))?;
    let mut found = BTreeMap::new();
    let mut pending = vec![value];
    let mut visited = BTreeSet::new();
    while let Some(index) = pending.pop() {
        collect_platform_descriptors(&index, &mut found)?;
        let manifests = index
            .get("manifests")
            .and_then(Value::as_sequence)
            .ok_or_else(|| BuildError::Metadata("OCI output is not an image index".to_owned()))?;
        for descriptor in manifests {
            if descriptor.get("platform").is_some() {
                continue;
            }
            let Some(digest) = descriptor.get("digest").and_then(Value::as_str) else {
                continue;
            };
            let digest = ImageDigest::new(digest).map_err(BuildError::Metadata)?;
            if !visited.insert(digest.as_str().to_owned()) {
                continue;
            }
            let hex = digest
                .as_str()
                .strip_prefix("sha256:")
                .expect("validated digest has sha256 prefix");
            let blob = layout.join("blobs").join("sha256").join(hex);
            let source = fs::read_to_string(&blob).map_err(|source| BuildError::Process {
                operation: "read nested OCI image index",
                source,
            })?;
            let nested: Value = serde_yaml::from_str(&source).map_err(|error| {
                BuildError::Metadata(format!("invalid nested OCI index JSON: {error}"))
            })?;
            if nested.get("manifests").is_some() {
                pending.push(nested);
            }
        }
    }
    finish_platform_digests(found, requested)
}

fn collect_platform_descriptors(
    value: &Value,
    found: &mut BTreeMap<Platform, ImageDigest>,
) -> Result<(), BuildError> {
    let manifests = value
        .get("manifests")
        .and_then(Value::as_sequence)
        .ok_or_else(|| BuildError::Metadata("image output is not a manifest list".to_owned()))?;
    for descriptor in manifests {
        let Some(platform) = descriptor.get("platform") else {
            continue;
        };
        let platform = match (
            platform.get("os").and_then(Value::as_str),
            platform.get("architecture").and_then(Value::as_str),
        ) {
            (Some("linux"), Some("amd64")) => Platform::LinuxAmd64,
            (Some("linux"), Some("arm64")) => Platform::LinuxArm64,
            _ => continue,
        };
        let digest = descriptor
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BuildError::Metadata(format!("manifest for {platform} has no digest"))
            })?;
        let digest = ImageDigest::new(digest).map_err(BuildError::Metadata)?;
        if let Some(previous) = found.insert(platform, digest.clone())
            && previous != digest
        {
            return Err(BuildError::Metadata(format!(
                "manifest list contains duplicate {platform} entries"
            )));
        }
    }
    Ok(())
}

fn finish_platform_digests(
    found: BTreeMap<Platform, ImageDigest>,
    requested: &[Platform],
) -> Result<Vec<PlatformDigest>, BuildError> {
    let missing = requested
        .iter()
        .copied()
        .filter(|platform| !found.contains_key(platform))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(BuildError::Metadata(format!(
            "manifest list is missing requested platform(s): {}",
            join_platforms(&missing)
        )));
    }
    Ok(requested
        .iter()
        .map(|platform| PlatformDigest {
            platform: *platform,
            digest: found[platform].clone(),
        })
        .collect())
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
    hash_field(&mut hasher, "repo-sandbox-template-plan-v2");
    hash_field(
        &mut hasher,
        &serde_json::to_string(plan).expect("template plan is serializable"),
    );
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

    struct ImmediatelyCancelled;

    impl Cancellation for ImmediatelyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn system_executor_bounds_descendants_that_inherit_output_pipes() {
        #[cfg(unix)]
        let invocation = ProcessInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 30 & wait".into()],
            current_dir: None,
        };
        #[cfg(windows)]
        let invocation = ProcessInvocation {
            program: "cmd".into(),
            args: vec![
                "/d".into(),
                "/s".into(),
                "/c".into(),
                "start \"\" /b cmd /d /s /c \"ping -n 30 127.0.0.1 >NUL\"".into(),
            ],
            current_dir: None,
        };
        let started = std::time::Instant::now();
        let output = SystemProcessExecutor
            .execute(&invocation, &ImmediatelyCancelled)
            .unwrap();
        assert!(output.interrupted);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone, Copy)]
    enum BuildBehavior {
        Success,
        MissingCrossPlatform,
        Failure,
        Interrupted,
        CreateConflict,
        ExistingName,
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
            let operation = invocation.args.get(1).map(String::as_str);
            if operation == Some("inspect")
                && invocation.args.iter().any(|arg| arg == "--bootstrap")
            {
                let platforms = if matches!(self.behavior, BuildBehavior::MissingCrossPlatform) {
                    "linux/amd64"
                } else {
                    "linux/amd64, linux/arm64, linux/arm64/v8"
                };
                return Ok(ProcessOutput {
                    exit_code: Some(0),
                    stdout: format!("Name: test\nStatus: running\nPlatforms: {platforms}"),
                    stderr: String::new(),
                    interrupted: false,
                });
            }
            if operation == Some("inspect") {
                if matches!(self.behavior, BuildBehavior::ExistingName) {
                    return Ok(ProcessOutput {
                        exit_code: Some(0),
                        stdout: "Name: repo-sandbox-task-7".to_owned(),
                        stderr: String::new(),
                        interrupted: false,
                    });
                }
                return Ok(ProcessOutput {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "ERROR: no builder found".to_owned(),
                    interrupted: false,
                });
            }
            if operation == Some("create") && matches!(self.behavior, BuildBehavior::CreateConflict)
            {
                return Ok(ProcessOutput {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "ERROR: existing instance already exists".to_owned(),
                    interrupted: false,
                });
            }
            let is_build = operation == Some("build");
            if is_build {
                match self.behavior {
                    BuildBehavior::Success | BuildBehavior::MissingCrossPlatform => {
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
                    BuildBehavior::CreateConflict => unreachable!("create fails before build"),
                    BuildBehavior::ExistingName => unreachable!("inspect fails before build"),
                }
            }
            if operation == Some("imagetools") {
                return Ok(ProcessOutput {
                    exit_code: Some(0),
                    stdout: format!(
                        r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{}","platform":{{"os":"linux","architecture":"amd64"}}}},{{"digest":"sha256:{}","platform":{{"os":"linux","architecture":"arm64","variant":"v8"}}}}]}}"#,
                        "b".repeat(64),
                        "c".repeat(64)
                    ),
                    stderr: String::new(),
                    interrupted: false,
                });
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
            target_platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
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
            execution: Default::default(),
        }
    }

    fn request(plan: &TemplatePlan, options: BuildOptions) -> BuildRequest<'_> {
        BuildRequest::environment(
            plan,
            Path::new("catalog root with spaces"),
            ImageRef::new("registry.test/repo-sandbox/rust-bazel:test").unwrap(),
            options,
        )
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let index = args.iter().position(|value| value == flag).unwrap();
        &args[index + 1]
    }

    fn amd64_adapter(executor: &FakeExecutor) -> BuildKit<&FakeExecutor> {
        BuildKit::new(executor).with_native_platform(Platform::LinuxAmd64)
    }

    #[test]
    fn cold_and_warm_builds_use_the_same_cache_contract_and_return_digest() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let adapter = amd64_adapter(&executor);
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
            assert_eq!(value_after(&invocation.args, "--target"), "environment");
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
    fn central_dockerfile_confines_assembly_inputs_to_named_stages() {
        let dockerfile = include_str!("../../../templates/rust-bazel/context/Dockerfile");
        let toolchain = dockerfile.find(" AS toolchain-build").unwrap();
        let environment = dockerfile.find(" AS environment").unwrap();
        assert!(toolchain < environment);
        assert!(dockerfile.contains("COPY --from=toolchain-build /usr/local/cargo/"));
        assert!(dockerfile.contains("COPY --from=toolchain-build /usr/local/rustup/"));
        assert!(dockerfile.contains("COPY --from=toolchain-build /toolchain/bin/bazel"));
        assert!(dockerfile.contains("COPY --from=toolchain-build /toolchain/bin/bazelisk"));
        assert!(dockerfile.contains("--mount=type=secret,id=github_token,required=false"));
        for cache in [
            "repo-sandbox-apt-",
            "repo-sandbox-cargo-registry-",
            "repo-sandbox-cargo-git-",
            "repo-sandbox-bazel-",
            "repo-sandbox-toolchain-downloads-",
        ] {
            assert!(dockerfile.contains(cache), "missing cache mount {cache}");
        }
        let final_stage = &dockerfile[environment..];
        assert!(
            !final_stage
                .contains("apt-get install --yes --no-install-recommends ca-certificates curl")
        );
        assert!(!final_stage.contains("/run/secrets/github_token"));
        assert!(final_stage.contains("target=/root/.cache/bazel"));
        assert!(final_stage.contains("rustup default \"$RUST_VERSION\""));
        assert!(final_stage.contains("rustup which --toolchain \"$RUST_VERSION\" rustc"));
        assert!(!final_stage.contains("/toolchain-downloads"));
        assert!(!final_stage.contains("BAZELISK_HOME="));

        let acceptance = include_str!("../../../scripts/docker/multistage-acceptance.sh");
        for contract in [
            "assert_cached_step \"$warm_log\" '[toolchain-build 2/2] RUN'",
            "cold_environment_identity",
            "environment_before_source_change",
            "environment_after_source_change",
            "restored_task_identity",
            "test -z \"$(find /root/.cache",
            "test ! -e /root/.cache/bazelisk",
        ] {
            assert!(
                acceptance.contains(contract),
                "missing acceptance contract {contract}"
            );
        }
    }

    #[test]
    fn target_is_a_structured_fixed_argument_not_a_caller_build_arg() {
        let plan = plan();
        let invocation = build_invocation(
            &request(
                &plan,
                BuildOptions {
                    build_args: BTreeMap::from([(
                        "TARGET".to_owned(),
                        "toolchain-build".to_owned(),
                    )]),
                    ..BuildOptions::default()
                },
            ),
            &[Platform::LinuxAmd64],
            Path::new("metadata.json"),
        )
        .unwrap();
        assert_eq!(value_after(&invocation.args, "--target"), "environment");
        assert!(
            invocation
                .args
                .windows(2)
                .any(|pair| pair == ["--build-arg", "TARGET=toolchain-build"])
        );
        assert!(
            !invocation
                .args
                .windows(2)
                .any(|pair| pair == ["--target", "toolchain-build"])
        );
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
        let adapter = amd64_adapter(&executor);
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
        let adapter = amd64_adapter(&executor);
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
    fn multi_platform_push_builds_and_verifies_both_manifest_digests() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let plan = plan();
        let result = BuildKit::new(&executor)
            .with_native_platform(Platform::LinuxAmd64)
            .build(
                request(
                    &plan,
                    BuildOptions {
                        output: ImageOutput::Push,
                        platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(result.platform_digests.len(), 2);
        assert_eq!(result.platform_digests[0].platform, Platform::LinuxAmd64);
        assert_eq!(
            result.platform_digests[0].digest.as_str(),
            format!("sha256:{}", "b".repeat(64))
        );
        assert_eq!(result.platform_digests[1].platform, Platform::LinuxArm64);

        let invocations = executor.invocations();
        let build = invocations
            .iter()
            .find(|call| call.args.get(1).map(String::as_str) == Some("build"))
            .unwrap();
        assert_eq!(
            value_after(&build.args, "--platform"),
            "linux/amd64,linux/arm64"
        );
        assert!(build.args.contains(&"--push".to_owned()));
        assert!(invocations.iter().any(|call| call.args.starts_with(&[
            "buildx".to_owned(),
            "imagetools".to_owned(),
            "inspect".to_owned(),
            "--raw".to_owned()
        ])));
    }

    #[test]
    fn plan_digest_build_arg_is_identical_for_single_and_multi_platform_invocations() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let plan = plan();
        let adapter = amd64_adapter(&executor);
        adapter
            .build(request(&plan, BuildOptions::default()), &NeverCancelled)
            .unwrap();
        adapter
            .build(
                request(
                    &plan,
                    BuildOptions {
                        output: ImageOutput::Push,
                        platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap();
        let builds = executor
            .invocations()
            .into_iter()
            .filter(|call| call.args.get(1).map(String::as_str) == Some("build"))
            .collect::<Vec<_>>();
        let digest = |invocation: &ProcessInvocation| {
            invocation
                .args
                .windows(2)
                .find(|pair| {
                    pair[0] == "--build-arg" && pair[1].starts_with("REPO_SANDBOX_PLAN_DIGEST=")
                })
                .unwrap()[1]
                .clone()
        };
        assert_eq!(digest(&builds[0]), digest(&builds[1]));
    }

    #[test]
    fn arm64_host_native_and_cross_platform_paths_are_deterministic() {
        let native_executor = FakeExecutor::new(BuildBehavior::Success);
        let mut arm_plan = plan();
        arm_plan.platform = Platform::LinuxArm64;
        let native = BuildKit::new(&native_executor)
            .with_native_platform(Platform::LinuxArm64)
            .build(request(&arm_plan, BuildOptions::default()), &NeverCancelled)
            .unwrap();
        assert_eq!(native.platform_digests[0].platform, Platform::LinuxArm64);
        assert_eq!(native_executor.invocations().len(), 1);
        assert_eq!(
            value_after(&native_executor.invocations()[0].args, "--platform"),
            "linux/arm64"
        );

        let cross_executor = FakeExecutor::new(BuildBehavior::Success);
        BuildKit::new(&cross_executor)
            .with_native_platform(Platform::LinuxArm64)
            .build(request(&plan(), BuildOptions::default()), &NeverCancelled)
            .unwrap();
        let cross_invocations = cross_executor.invocations();
        assert_eq!(cross_invocations.len(), 2);
        assert_eq!(
            cross_invocations[0].args,
            ["buildx", "inspect", "--bootstrap"]
        );
        assert_eq!(
            value_after(&cross_invocations[1].args, "--platform"),
            "linux/amd64"
        );
    }

    #[test]
    fn multi_platform_load_and_duplicate_targets_fail_before_docker() {
        let executor = FakeExecutor::new(BuildBehavior::Success);
        let plan = plan();
        for platforms in [
            vec![Platform::LinuxAmd64, Platform::LinuxArm64],
            vec![Platform::LinuxAmd64, Platform::LinuxAmd64],
        ] {
            let error = BuildKit::new(&executor)
                .build(
                    request(
                        &plan,
                        BuildOptions {
                            platforms,
                            ..BuildOptions::default()
                        },
                    ),
                    &NeverCancelled,
                )
                .unwrap_err();
            assert!(matches!(error, BuildError::InvalidRequest(_)));
        }
        assert!(executor.invocations().is_empty());
    }

    #[test]
    fn missing_qemu_or_native_node_fails_before_build() {
        let executor = FakeExecutor::new(BuildBehavior::MissingCrossPlatform);
        let mut arm_plan = plan();
        arm_plan.platform = Platform::LinuxArm64;
        let error = amd64_adapter(&executor)
            .build(request(&arm_plan, BuildOptions::default()), &NeverCancelled)
            .unwrap_err();
        assert!(matches!(error, BuildError::Capability(_)));
        assert!(error.to_string().contains("QEMU/binfmt"));
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].args.contains(&"--bootstrap".to_owned()));
        assert!(
            !invocations
                .iter()
                .any(|call| call.args.get(1).map(String::as_str) == Some("build"))
        );
    }

    #[test]
    fn manifest_validation_rejects_missing_requested_platform() {
        let source = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
            "d".repeat(64)
        );
        let error = parse_platform_digests(&source, &[Platform::LinuxAmd64, Platform::LinuxArm64])
            .unwrap_err();
        assert!(error.to_string().contains("linux/arm64"));
    }

    #[test]
    fn buildkit_failure_preserves_reason_and_exit_code() {
        let executor = FakeExecutor::new(BuildBehavior::Failure);
        let plan = plan();
        let error = amd64_adapter(&executor)
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
        let error = amd64_adapter(&executor)
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
        assert_eq!(invocations.len(), 4);
        assert_eq!(
            invocations[3].args,
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
        let _ = amd64_adapter(&executor).build(
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

    #[test]
    fn create_name_race_never_removes_the_unowned_builder() {
        let executor = FakeExecutor::new(BuildBehavior::CreateConflict);
        let plan = plan();
        let error = amd64_adapter(&executor)
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
        assert_eq!(error.exit_code(), Some(1));
        assert!(error.stderr().unwrap().contains("already exists"));
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 2);
        assert_eq!(
            invocations[0].args,
            ["buildx", "inspect", "repo-sandbox-task-7"]
        );
        assert_eq!(invocations[1].args[1], "create");
        assert!(invocations.iter().all(|invocation| {
            !invocation
                .args
                .iter()
                .any(|argument| matches!(argument.as_str(), "rm" | "prune" | "system"))
        }));
    }

    #[test]
    fn preexisting_ephemeral_name_is_rejected_without_create_or_remove() {
        let executor = FakeExecutor::new(BuildBehavior::ExistingName);
        let plan = plan();
        let error = amd64_adapter(&executor)
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
        assert!(matches!(error, BuildError::InvalidRequest(_)));
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].args,
            ["buildx", "inspect", "repo-sandbox-task-7"]
        );
    }

    #[test]
    fn successful_create_then_build_failure_removes_owned_builder() {
        let executor = FakeExecutor::new(BuildBehavior::Failure);
        let plan = plan();
        let error = amd64_adapter(&executor)
            .build(
                request(
                    &plan,
                    BuildOptions {
                        builder: Builder::Ephemeral {
                            name: "repo-sandbox-task-8".to_owned(),
                        },
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap_err();
        assert_eq!(error.exit_code(), Some(42));
        let invocations = executor.invocations();
        assert_eq!(invocations.len(), 4);
        assert_eq!(
            invocations[3].args,
            ["buildx", "rm", "--force", "repo-sandbox-task-8"]
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
FROM ${BASE_IMAGE} AS environment
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
                    BuildRequest::environment(
                        &plan,
                        catalog.path(),
                        image.clone(),
                        BuildOptions {
                            progress: Progress::Plain,
                            ..BuildOptions::default()
                        },
                    ),
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

    /// Optional end-to-end check for both executable architectures and a local OCI index.
    #[test]
    #[ignore = "requires Docker buildx, busybox:1.36, and amd64/arm64 native nodes or QEMU/binfmt"]
    fn docker_two_architecture_smoke_and_oci_manifest() {
        let catalog = tempfile::tempdir().unwrap();
        let context = catalog.path().join("context");
        fs::create_dir(&context).unwrap();
        fs::write(
            context.join("Dockerfile"),
            r#"FROM busybox:1.36 AS environment
RUN uname -m > /built-architecture
CMD ["cat", "/built-architecture"]
"#,
        )
        .unwrap();
        let mut smoke_plan = plan();
        smoke_plan.base_image = "busybox:1.36".to_owned();
        smoke_plan.build_context = PathBuf::from("context");
        smoke_plan.stages.clear();
        let adapter = BuildKit::new(SystemProcessExecutor);
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
        let mut images = Vec::new();
        for (platform, expected) in [
            (Platform::LinuxAmd64, "x86_64"),
            (Platform::LinuxArm64, "aarch64"),
        ] {
            smoke_plan.platform = platform;
            let image = ImageRef::new(format!(
                "repo-sandbox-issue10-{}:{}",
                platform.as_str().replace('/', "-"),
                std::process::id()
            ))
            .unwrap();
            let built = adapter
                .build(
                    BuildRequest::environment(
                        &smoke_plan,
                        catalog.path(),
                        image.clone(),
                        BuildOptions {
                            progress: Progress::Plain,
                            ..BuildOptions::default()
                        },
                    ),
                    &NeverCancelled,
                )
                .unwrap();
            assert_eq!(built.platform_digests[0].platform, platform);
            let smoke = run(vec![
                "run".to_owned(),
                "--rm".to_owned(),
                "--platform".to_owned(),
                platform.to_string(),
                image.to_string(),
            ]);
            assert_eq!(smoke.exit_code, Some(0), "{}", smoke.stderr);
            assert_eq!(smoke.stdout.trim(), expected);
            images.push(image);
        }

        let output = catalog.path().join("multi-arch-oci");
        let multi = adapter
            .build(
                BuildRequest::environment(
                    &smoke_plan,
                    catalog.path(),
                    ImageRef::new("repo-sandbox-issue10:multi").unwrap(),
                    BuildOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::OciDirectory(output),
                        platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(
            multi
                .platform_digests
                .iter()
                .map(|entry| entry.platform)
                .collect::<Vec<_>>(),
            [Platform::LinuxAmd64, Platform::LinuxArm64]
        );
        for image in images {
            let _ = run(vec![
                "image".to_owned(),
                "rm".to_owned(),
                "--force".to_owned(),
                image.to_string(),
            ]);
        }
    }

    /// Optional registry check. Authentication and registry lifecycle remain Issue #11 concerns.
    #[test]
    #[ignore = "requires REPO_SANDBOX_MULTIARCH_TEST_IMAGE naming a writable disposable registry tag"]
    fn docker_pushed_tag_contains_and_runs_both_platforms() {
        let image = std::env::var("REPO_SANDBOX_MULTIARCH_TEST_IMAGE")
            .expect("set REPO_SANDBOX_MULTIARCH_TEST_IMAGE to a disposable writable tag");
        let catalog = tempfile::tempdir().unwrap();
        fs::write(
            catalog.path().join("Dockerfile"),
            "FROM busybox:1.36 AS environment\nCMD [\"uname\", \"-m\"]\n",
        )
        .unwrap();
        let mut smoke_plan = plan();
        smoke_plan.base_image = "busybox:1.36".to_owned();
        smoke_plan.build_context = PathBuf::from(".");
        smoke_plan.stages.clear();
        let built = BuildKit::new(SystemProcessExecutor)
            .build(
                BuildRequest::environment(
                    &smoke_plan,
                    catalog.path(),
                    ImageRef::new(&image).unwrap(),
                    BuildOptions {
                        progress: Progress::Plain,
                        output: ImageOutput::Push,
                        platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                        ..BuildOptions::default()
                    },
                ),
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(built.platform_digests.len(), 2);
        for (platform, expected) in [
            (Platform::LinuxAmd64, "x86_64"),
            (Platform::LinuxArm64, "aarch64"),
        ] {
            let smoke = SystemProcessExecutor
                .execute(
                    &ProcessInvocation {
                        program: "docker".to_owned(),
                        args: vec![
                            "run".to_owned(),
                            "--rm".to_owned(),
                            "--platform".to_owned(),
                            platform.to_string(),
                            image.clone(),
                        ],
                        current_dir: None,
                    },
                    &NeverCancelled,
                )
                .unwrap();
            assert_eq!(smoke.exit_code, Some(0), "{}", smoke.stderr);
            assert_eq!(smoke.stdout.trim(), expected);
        }
    }
}
