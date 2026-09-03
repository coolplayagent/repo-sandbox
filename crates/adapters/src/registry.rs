//! Generic OCI registry distribution through the Docker credential/helper boundary.
//!
//! This module owns no registry service and never persists credentials itself. Docker
//! reads/writes its configured credential helper; explicit passwords are runtime-only
//! values sent on stdin.

use crate::buildkit::{Cancellation, NeverCancelled};
use crate::snapshot::{ProcessTree, configure_process_tree};
use repo_sandbox_core::build::{ImageDigest, ImageRef, PlatformDigest};
use repo_sandbox_core::config::Platform;
use repo_sandbox_core::registry::{
    PublishRequest, PublishedImage, PullRequest, PulledImage, RegistryTag,
};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub interrupted: bool,
}

/// Injectable process boundary. Secret input is deliberately separate from argv and Debug.
pub trait RegistryExecutor {
    fn execute(
        &self,
        invocation: &RegistryInvocation,
        stdin: Option<&[u8]>,
        cancellation: &dyn Cancellation,
    ) -> io::Result<RegistryOutput>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRegistryExecutor;

impl RegistryExecutor for SystemRegistryExecutor {
    fn execute(
        &self,
        invocation: &RegistryInvocation,
        stdin: Option<&[u8]>,
        cancellation: &dyn Cancellation,
    ) -> io::Result<RegistryOutput> {
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(
                invocation
                    .current_dir
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new(".")),
            )
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn()?;
        let process_tree = ProcessTree::attach(&mut child).inspect_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
        })?;
        let stdin_writer = stdin.map(|secret| {
            let mut secret = secret.to_vec();
            let mut pipe = child.stdin.take().expect("piped stdin");
            thread::spawn(move || {
                let result = pipe.write_all(&secret);
                secret.fill(0);
                result
            })
        });
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_reader = thread::spawn(move || read_output(stdout));
        let stderr_reader = thread::spawn(move || read_output(stderr));
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
        // A registry CLI can exit while its Buildx plugin or credential helper
        // still owns the pipes. Terminate the creation-time tree before joining.
        process_tree.terminate();
        if let Some(writer) = stdin_writer {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(io::Error::other("registry stdin writer panicked")),
            }
        }
        Ok(RegistryOutput {
            exit_code: status.code(),
            stdout: join_output(stdout_reader)?,
            stderr: join_output(stderr_reader)?,
            interrupted,
        })
    }
}

fn read_output(mut input: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_output(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<String> {
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("registry process output reader panicked"))??;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Runtime secret with redacted formatting and best-effort in-memory clearing.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(value: impl Into<Vec<u8>>) -> Result<Self, RegistryError> {
        let value = value.into();
        if value.is_empty() || value.contains(&b'\n') || value.contains(&b'\r') {
            return Err(RegistryError::invalid_request(
                "registry password/token must be non-empty and single-line".into(),
            ));
        }
        Ok(Self(value))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Debug for Secret {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug)]
pub enum RegistryCredential {
    /// Ask Docker to use the credential helper configured outside repo-sandbox.
    CredentialHelper { access_probe: ImageRef },
    /// Validate and store through `docker login`; the secret is passed only on stdin.
    Password {
        username: String,
        secret: Secret,
        /// Docker credential helper suffix, for example `desktop` or `pass`.
        credential_helper: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryErrorKind {
    InvalidRequest,
    Authentication,
    Network,
    Command,
    Interrupted,
    DigestMismatch,
    Manifest,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RegistryError {
    kind: RegistryErrorKind,
    message: String,
}

impl RegistryError {
    /// Build a safe adapter error. The message must already have credentials redacted.
    pub fn new(kind: RegistryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_request(message: String) -> Self {
        Self {
            kind: RegistryErrorKind::InvalidRequest,
            message,
        }
    }

    pub const fn kind(&self) -> RegistryErrorKind {
        self.kind
    }
}

impl Display for RegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "OCI registry {:?}: {}", self.kind, self.message)
    }
}

impl Error for RegistryError {}

/// Vendor-neutral boundary. A future SWR adapter can implement this without changing callers.
pub trait OciRegistry {
    fn login(
        &self,
        registry: &str,
        credential: &RegistryCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<(), RegistryError>;

    fn publish(
        &self,
        request: &PublishRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<PublishedImage, RegistryError>;

    fn pull_and_verify(
        &self,
        request: &PullRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<PulledImage, RegistryError>;
}

pub struct DockerRegistry<E> {
    executor: E,
}

impl<E> DockerRegistry<E> {
    pub const fn new(executor: E) -> Self {
        Self { executor }
    }
}

impl<E: RegistryExecutor> DockerRegistry<E> {
    /// Publish while reporting only remote references whose manifest copy and
    /// digest/platform verification have both completed successfully.
    pub fn publish_with_progress(
        &self,
        request: &PublishRequest,
        cancellation: &dyn Cancellation,
        mut progress: impl FnMut(&PublishedImage),
    ) -> Result<PublishedImage, RegistryError> {
        validate_publish(request)?;
        let content_tag = RegistryTag::for_digest(&request.digest);
        let immutable = request.repository.tagged(&content_tag);
        let source = digest_ref(&request.source, &request.digest)?;
        self.copy_manifest(&source, &immutable, &[], cancellation)?;
        let mut published = PublishedImage {
            immutable: immutable.clone(),
            aliases: Vec::new(),
            digest: request.digest.clone(),
            platform_digests: request.platform_digests.clone(),
        };
        // A successful copy is an irreversible remote fact even if the
        // following verification request fails. Surface it immediately.
        progress(&published);
        let inspected = self.inspect_digest(&immutable, &request.platform_digests, cancellation)?;
        ensure_digest(&request.digest, &inspected.digest, "published content tag")?;
        published.digest = inspected.digest;
        published.platform_digests = inspected.platforms;
        progress(&published);
        for alias in &request.aliases {
            let target = request.repository.tagged(alias);
            let pinned = digest_ref(&immutable, &request.digest)?;
            self.copy_manifest(&pinned, &target, &[], cancellation)?;
            published.aliases.push(target.clone());
            progress(&published);
            let alias_manifest =
                self.inspect_digest(&target, &request.platform_digests, cancellation)?;
            ensure_digest(&request.digest, &alias_manifest.digest, "published alias")?;
            progress(&published);
        }
        Ok(published)
    }
}

impl DockerRegistry<SystemRegistryExecutor> {
    pub fn login_default(
        &self,
        registry: &str,
        credential: &RegistryCredential,
    ) -> Result<(), RegistryError> {
        self.login(registry, credential, &NeverCancelled)
    }
}

impl<E: RegistryExecutor> OciRegistry for DockerRegistry<E> {
    fn login(
        &self,
        registry: &str,
        credential: &RegistryCredential,
        cancellation: &dyn Cancellation,
    ) -> Result<(), RegistryError> {
        validate_registry_host(registry)?;
        match credential {
            RegistryCredential::CredentialHelper { access_probe } => {
                if !access_probe.as_str().starts_with(&format!("{registry}/")) {
                    return Err(RegistryError::invalid_request(
                        "credential-helper access probe must belong to the requested registry"
                            .into(),
                    ));
                }
                self.inspect_digest(access_probe, &[], cancellation)
                    .map(|_| ())
            }
            RegistryCredential::Password {
                username,
                secret,
                credential_helper,
            } => {
                if username.trim().is_empty() || username.contains(char::is_whitespace) {
                    return Err(RegistryError::invalid_request(
                        "registry username must be non-empty and contain no whitespace".into(),
                    ));
                }
                if credential_helper.is_empty()
                    || !credential_helper
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                {
                    return Err(RegistryError::invalid_request(
                        "credential helper must contain only letters, digits, or '-'".into(),
                    ));
                }
                // Force the selected helper and prevent Docker's config-file auth fallback.
                // This temporary config contains only a helper name, never a credential.
                let docker_config = tempfile::tempdir().map_err(|source| {
                    RegistryError::new(
                        RegistryErrorKind::Command,
                        format!("create temporary Docker credential config: {source}"),
                    )
                })?;
                fs::write(
                    docker_config.path().join("config.json"),
                    format!(r#"{{"credsStore":"{credential_helper}"}}"#),
                )
                .map_err(|source| {
                    RegistryError::new(
                        RegistryErrorKind::Command,
                        format!("write temporary Docker credential config: {source}"),
                    )
                })?;
                let invocation = docker(&[
                    "--config".into(),
                    docker_config.path().to_string_lossy().into_owned(),
                    "login".into(),
                    registry.into(),
                    "--username".into(),
                    username.clone(),
                    "--password-stdin".into(),
                ]);
                self.run("login", &invocation, Some(secret), cancellation)
                    .map(|_| ())
            }
        }
    }

    fn publish(
        &self,
        request: &PublishRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<PublishedImage, RegistryError> {
        self.publish_with_progress(request, cancellation, |_| {})
    }

    fn pull_and_verify(
        &self,
        request: &PullRequest,
        cancellation: &dyn Cancellation,
    ) -> Result<PulledImage, RegistryError> {
        if request.expected_platforms.is_empty() {
            return Err(RegistryError::invalid_request(
                "at least one pull platform is required".into(),
            ));
        }
        reject_duplicates(request.expected_platforms.iter().copied())?;
        let inspected = self.inspect_digest(&request.image, &[], cancellation)?;
        ensure_digest(&request.expected_digest, &inspected.digest, "remote image")?;
        let available: BTreeSet<_> = inspected
            .platforms
            .iter()
            .map(|item| item.platform)
            .collect();
        if inspected.platforms.is_empty() {
            if request.expected_platforms.len() != 1 {
                return Err(RegistryError {
                    kind: RegistryErrorKind::Manifest,
                    message:
                        "a single-platform manifest can satisfy exactly one requested platform"
                            .into(),
                });
            }
        } else {
            for platform in &request.expected_platforms {
                if !available.contains(platform) {
                    return Err(RegistryError {
                        kind: RegistryErrorKind::Manifest,
                        message: format!("remote manifest is missing {platform}"),
                    });
                }
            }
        }
        let pinned = digest_ref(&request.image, &request.expected_digest)?;
        for platform in &request.expected_platforms {
            let invocation = docker(&[
                "pull".into(),
                "--platform".into(),
                platform.to_string(),
                pinned.clone(),
            ]);
            self.run("pull platform", &invocation, None, cancellation)?;
            let inspect = docker(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Os}}/{{.Architecture}}".into(),
                pinned.clone(),
            ]);
            let local = self.run("verify pulled platform", &inspect, None, cancellation)?;
            if local.stdout.trim() != platform.as_str() {
                return Err(RegistryError {
                    kind: RegistryErrorKind::Manifest,
                    message: format!(
                        "pulled image platform changed: expected {platform}, got {}",
                        local.stdout.trim()
                    ),
                });
            }
        }
        // Re-inspection catches mutable-tag races between the first inspection and pulls.
        let after = self.inspect_digest(&request.image, &[], cancellation)?;
        ensure_digest(
            &request.expected_digest,
            &after.digest,
            "remote image after pull",
        )?;
        Ok(PulledImage {
            image: request.image.clone(),
            digest: after.digest,
            platforms: request.expected_platforms.clone(),
        })
    }
}

struct InspectedManifest {
    digest: ImageDigest,
    platforms: Vec<PlatformDigest>,
}

impl<E: RegistryExecutor> DockerRegistry<E> {
    fn copy_manifest(
        &self,
        source: &str,
        target: &ImageRef,
        secrets: &[&Secret],
        cancellation: &dyn Cancellation,
    ) -> Result<(), RegistryError> {
        let invocation = docker(&[
            "buildx".into(),
            "imagetools".into(),
            "create".into(),
            "--tag".into(),
            target.to_string(),
            source.into(),
        ]);
        self.run("tag and push manifest", &invocation, None, cancellation)
            .map(|_| ())
            .map_err(|error| redact_error(error, secrets))
    }

    fn inspect_digest(
        &self,
        image: &ImageRef,
        expected: &[PlatformDigest],
        cancellation: &dyn Cancellation,
    ) -> Result<InspectedManifest, RegistryError> {
        let digest_invocation = docker(&[
            "buildx".into(),
            "imagetools".into(),
            "inspect".into(),
            image.to_string(),
        ]);
        let described = self.run(
            "inspect manifest descriptor",
            &digest_invocation,
            None,
            cancellation,
        )?;
        let digest = parse_descriptor_digest(&described.stdout)?;
        let raw_invocation = docker(&[
            "buildx".into(),
            "imagetools".into(),
            "inspect".into(),
            "--raw".into(),
            image.to_string(),
        ]);
        let raw = self.run("inspect raw manifest", &raw_invocation, None, cancellation)?;
        let mut platforms = parse_platforms(&raw.stdout)?;
        if platforms.is_empty() && expected.len() == 1 && expected[0].digest == digest {
            // OCI image manifests do not carry an os/architecture descriptor;
            // the exact top-level digest plus BuildKit's single requested
            // platform is the complete immutable evidence in this case.
            platforms = expected.to_vec();
        }
        if !expected.is_empty() {
            verify_platforms(&platforms, expected)?;
        }
        Ok(InspectedManifest { digest, platforms })
    }

    fn run(
        &self,
        operation: &str,
        invocation: &RegistryInvocation,
        secret: Option<&Secret>,
        cancellation: &dyn Cancellation,
    ) -> Result<RegistryOutput, RegistryError> {
        let output = self
            .executor
            .execute(invocation, secret.map(Secret::as_bytes), cancellation)
            .map_err(|source| RegistryError {
                kind: classify_io(&source),
                message: format!("{operation}: {source}"),
            })?;
        if output.interrupted {
            return Err(RegistryError {
                kind: RegistryErrorKind::Interrupted,
                message: format!(
                    "{operation} was interrupted: {}",
                    sanitize(&output.stderr, secret)
                ),
            });
        }
        if output.exit_code != Some(0) {
            let stderr = sanitize(&output.stderr, secret);
            return Err(RegistryError {
                kind: classify_stderr(&stderr),
                message: format!(
                    "{operation} exited with {}: {}",
                    output
                        .exit_code
                        .map_or_else(|| "no exit code".into(), |code| code.to_string()),
                    stderr.trim()
                ),
            });
        }
        Ok(output)
    }
}

fn docker(args: &[String]) -> RegistryInvocation {
    RegistryInvocation {
        program: "docker".into(),
        args: args.to_vec(),
        current_dir: None,
    }
}

fn validate_registry_host(registry: &str) -> Result<(), RegistryError> {
    if registry.is_empty()
        || registry.contains('/')
        || registry.contains("://")
        || registry.contains(char::is_whitespace)
    {
        Err(RegistryError::invalid_request(
            "registry must be a host with optional port".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_publish(request: &PublishRequest) -> Result<(), RegistryError> {
    if request.platform_digests.is_empty() {
        return Err(RegistryError::invalid_request(
            "published image must contain at least one platform digest".into(),
        ));
    }
    reject_duplicates(request.platform_digests.iter().map(|item| item.platform))?;
    let mut tags = BTreeSet::new();
    let content = RegistryTag::for_digest(&request.digest);
    tags.insert(content.as_str());
    for alias in &request.aliases {
        if !tags.insert(alias.as_str()) {
            return Err(RegistryError::invalid_request(format!(
                "duplicate registry tag `{alias}`"
            )));
        }
    }
    Ok(())
}

fn reject_duplicates(items: impl IntoIterator<Item = Platform>) -> Result<(), RegistryError> {
    let mut found = BTreeSet::new();
    for item in items {
        if !found.insert(item) {
            return Err(RegistryError::invalid_request(format!(
                "duplicate platform `{item}`"
            )));
        }
    }
    Ok(())
}

fn digest_ref(image: &ImageRef, digest: &ImageDigest) -> Result<String, RegistryError> {
    if image.as_str().contains('@') {
        return Err(RegistryError::invalid_request(
            "image reference must not already contain a digest".into(),
        ));
    }
    Ok(format!("{image}@{digest}"))
}

fn ensure_digest(
    expected: &ImageDigest,
    actual: &ImageDigest,
    subject: &str,
) -> Result<(), RegistryError> {
    if expected == actual {
        Ok(())
    } else {
        Err(RegistryError {
            kind: RegistryErrorKind::DigestMismatch,
            message: format!("{subject} digest changed: expected {expected}, got {actual}"),
        })
    }
}

fn parse_descriptor_digest(output: &str) -> Result<ImageDigest, RegistryError> {
    // Only the unindented top-level field is the descriptor selected by the name.
    // Child entries may also contain indented `Digest:` fields.
    let values: Vec<_> = output
        .lines()
        .filter_map(|line| line.strip_prefix("Digest:"))
        .map(str::trim)
        .collect();
    match values.as_slice() {
        [value] => ImageDigest::new(*value).map_err(|message| RegistryError {
            kind: RegistryErrorKind::Manifest,
            message: format!("registry reported an invalid descriptor digest: {message}"),
        }),
        [] => Err(RegistryError {
            kind: RegistryErrorKind::Manifest,
            message: "registry inspection omitted the top-level descriptor digest".into(),
        }),
        _ => Err(RegistryError {
            kind: RegistryErrorKind::Manifest,
            message: "registry inspection reported multiple top-level descriptor digests".into(),
        }),
    }
}

fn parse_platforms(raw: &str) -> Result<Vec<PlatformDigest>, RegistryError> {
    let value: Value = serde_yaml::from_str(raw).map_err(|error| RegistryError {
        kind: RegistryErrorKind::Manifest,
        message: format!("registry returned invalid manifest JSON: {error}"),
    })?;
    let Some(manifests) = value.get("manifests").and_then(Value::as_sequence) else {
        return Ok(Vec::new());
    };
    let mut found = BTreeMap::new();
    for descriptor in manifests {
        let Some(platform) = descriptor.get("platform") else {
            continue;
        };
        if platform.get("os").and_then(Value::as_str) != Some("linux") {
            continue;
        }
        let parsed = match platform.get("architecture").and_then(Value::as_str) {
            Some("amd64") => Platform::LinuxAmd64,
            Some("arm64") => Platform::LinuxArm64,
            _ => continue,
        };
        let digest = descriptor
            .get("digest")
            .and_then(Value::as_str)
            .ok_or_else(|| RegistryError {
                kind: RegistryErrorKind::Manifest,
                message: format!("manifest descriptor for {parsed} has no digest"),
            })?;
        let digest = ImageDigest::new(digest).map_err(|message| RegistryError {
            kind: RegistryErrorKind::Manifest,
            message,
        })?;
        if found.insert(parsed, digest).is_some() {
            return Err(RegistryError {
                kind: RegistryErrorKind::Manifest,
                message: format!("manifest contains duplicate {parsed}"),
            });
        }
    }
    Ok(found
        .into_iter()
        .map(|(platform, digest)| PlatformDigest { platform, digest })
        .collect())
}

fn verify_platforms(
    actual: &[PlatformDigest],
    expected: &[PlatformDigest],
) -> Result<(), RegistryError> {
    let actual: BTreeMap<_, _> = actual
        .iter()
        .map(|item| (item.platform, &item.digest))
        .collect();
    for item in expected {
        match actual.get(&item.platform) {
            Some(digest) if *digest == &item.digest => {}
            Some(digest) => {
                return Err(RegistryError {
                    kind: RegistryErrorKind::DigestMismatch,
                    message: format!(
                        "{} manifest digest changed: expected {}, got {}",
                        item.platform, item.digest, digest
                    ),
                });
            }
            None => {
                return Err(RegistryError {
                    kind: RegistryErrorKind::Manifest,
                    message: format!("remote manifest is missing {}", item.platform),
                });
            }
        }
    }
    Ok(())
}

fn classify_io(error: &io::Error) -> RegistryErrorKind {
    match error.kind() {
        io::ErrorKind::TimedOut
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => RegistryErrorKind::Network,
        _ => RegistryErrorKind::Command,
    }
}

fn classify_stderr(stderr: &str) -> RegistryErrorKind {
    let lower = stderr.to_ascii_lowercase();
    if [
        "unauthorized",
        "authentication required",
        "no basic auth credentials",
        "requested access is denied",
        "denied:",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        RegistryErrorKind::Authentication
    } else if [
        "connection refused",
        "i/o timeout",
        "timed out",
        "tls handshake timeout",
        "no such host",
        "network is unreachable",
        "network unreachable",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        RegistryErrorKind::Network
    } else {
        RegistryErrorKind::Command
    }
}

fn redact_error(mut error: RegistryError, secrets: &[&Secret]) -> RegistryError {
    error.message = sanitize(&error.message, secrets.iter().copied());
    error
}

fn sanitize<'a>(value: &str, secrets: impl IntoIterator<Item = &'a Secret>) -> String {
    let mut redacted = value.to_owned();
    for secret in secrets {
        if let Ok(secret) = std::str::from_utf8(secret.as_bytes()) {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    let mut words: VecDeque<String> = redacted.split_whitespace().map(str::to_owned).collect();
    let mut safe = Vec::with_capacity(words.len());
    let mut hide_next = false;
    while let Some(word) = words.pop_front() {
        let lower = word.to_ascii_lowercase();
        if hide_next {
            safe.push("[REDACTED]".to_owned());
            hide_next = false;
        } else if lower == "bearer" || lower == "authorization:" {
            safe.push(word);
            hide_next = true;
        } else if ["token=", "token:", "password=", "password:"]
            .iter()
            .any(|marker| lower.starts_with(marker))
        {
            let split = word.find(['=', ':']).unwrap();
            safe.push(format!("{}[REDACTED]", &word[..=split]));
        } else {
            safe.push(redact_url_userinfo(&word));
        }
    }
    safe.join(" ")
}

fn redact_url_userinfo(word: &str) -> String {
    let Some(scheme) = word.find("://") else {
        return word.to_owned();
    };
    let after = scheme + 3;
    let Some(at_relative) = word[after..].find('@') else {
        return word.to_owned();
    };
    let at = after + at_relative;
    if word[after..at].contains(':') {
        format!("{}[REDACTED]{}", &word[..after], &word[at..])
    } else {
        word.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_sandbox_core::registry::RegistryRepository;
    use sha2::Digest as _;
    use std::sync::Mutex;

    const ROOT: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const AMD: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ARM: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    struct ImmediatelyCancelled;

    impl Cancellation for ImmediatelyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn system_registry_executor_bounds_helpers_that_inherit_output_pipes() {
        #[cfg(unix)]
        let invocation = RegistryInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 30 & wait".into()],
            current_dir: None,
        };
        #[cfg(windows)]
        let invocation = RegistryInvocation {
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
        let output = SystemRegistryExecutor
            .execute(&invocation, None, &ImmediatelyCancelled)
            .unwrap();
        assert!(output.interrupted);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn system_registry_executor_cancels_a_blocked_secret_stdin_writer() {
        struct CancelSoon(std::time::Instant);
        impl Cancellation for CancelSoon {
            fn is_cancelled(&self) -> bool {
                self.0.elapsed() >= Duration::from_millis(100)
            }
        }
        #[cfg(unix)]
        let invocation = RegistryInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            current_dir: None,
        };
        #[cfg(windows)]
        let invocation = RegistryInvocation {
            program: "cmd".into(),
            args: vec![
                "/d".into(),
                "/s".into(),
                "/c".into(),
                "ping -n 30 127.0.0.1 >NUL".into(),
            ],
            current_dir: None,
        };
        let secret = vec![b'x'; 4 * 1024 * 1024];
        let started = std::time::Instant::now();
        let output = SystemRegistryExecutor
            .execute(&invocation, Some(&secret), &CancelSoon(started))
            .unwrap();
        assert!(output.interrupted);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn system_registry_executor_bounds_broken_pipe_from_unread_secret_stdin() {
        #[cfg(unix)]
        let invocation = RegistryInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            current_dir: None,
        };
        #[cfg(windows)]
        let invocation = RegistryInvocation {
            program: "cmd".into(),
            args: vec!["/d".into(), "/c".into(), "exit 0".into()],
            current_dir: None,
        };
        let secret = vec![b'x'; 4 * 1024 * 1024];
        let started = std::time::Instant::now();
        let output = SystemRegistryExecutor
            .execute(&invocation, Some(&secret), &NeverCancelled)
            .unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    struct FakeExecutor {
        outputs: Mutex<VecDeque<RegistryOutput>>,
        invocations: Mutex<Vec<(RegistryInvocation, bool)>>,
    }

    impl FakeExecutor {
        fn new(outputs: Vec<RegistryOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                invocations: Mutex::new(Vec::new()),
            }
        }
    }

    impl RegistryExecutor for FakeExecutor {
        fn execute(
            &self,
            invocation: &RegistryInvocation,
            stdin: Option<&[u8]>,
            _cancellation: &dyn Cancellation,
        ) -> io::Result<RegistryOutput> {
            self.invocations
                .lock()
                .unwrap()
                .push((invocation.clone(), stdin.is_some()));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::other("missing fake output"))
        }
    }

    fn ok(stdout: impl Into<String>) -> RegistryOutput {
        RegistryOutput {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            interrupted: false,
        }
    }

    fn failed(stderr: impl Into<String>) -> RegistryOutput {
        RegistryOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
            interrupted: false,
        }
    }

    fn multiarch() -> String {
        format!(
            r#"{{"schemaVersion":2,"manifests":[{{"digest":"{AMD}","platform":{{"os":"linux","architecture":"amd64"}}}},{{"digest":"{ARM}","platform":{{"os":"linux","architecture":"arm64","variant":"v8"}}}}]}}
"#
        )
    }

    fn root_digest() -> ImageDigest {
        ImageDigest::new(ROOT).unwrap()
    }

    fn described() -> String {
        format!(
            "Name: registry.test/team/image:tag\nMediaType: application/vnd.oci.image.index.v1+json\nDigest: {ROOT}\n\nManifests:\n  Name: child@{AMD}\n  Digest: {AMD}\n"
        )
    }

    fn single_manifest() -> String {
        "{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.manifest.v1+json\",\"config\":{},\"layers\":[]}\n".into()
    }

    fn platform_digests() -> Vec<PlatformDigest> {
        vec![
            PlatformDigest {
                platform: Platform::LinuxAmd64,
                digest: ImageDigest::new(AMD).unwrap(),
            },
            PlatformDigest {
                platform: Platform::LinuxArm64,
                digest: ImageDigest::new(ARM).unwrap(),
            },
        ]
    }

    #[test]
    fn password_login_uses_stdin_and_redacts_all_surfaces() {
        let password = "top-secret-token";
        let executor = FakeExecutor::new(vec![ok("")]);
        let registry = DockerRegistry::new(executor);
        let credential = RegistryCredential::Password {
            username: "robot".into(),
            secret: Secret::new(password).unwrap(),
            credential_helper: "test-helper".into(),
        };
        registry
            .login("registry.test", &credential, &NeverCancelled)
            .unwrap();
        let calls = registry.executor.invocations.lock().unwrap();
        assert_eq!(calls[0].0.args[0], "--config");
        assert_eq!(
            &calls[0].0.args[2..],
            [
                "login",
                "registry.test",
                "--username",
                "robot",
                "--password-stdin"
            ]
        );
        assert!(calls[0].1);
        assert!(!format!("{:?}", calls[0].0).contains(password));
        assert!(!format!("{credential:?}").contains(password));
    }

    #[test]
    fn failures_distinguish_authentication_and_network_and_hide_tokens() {
        let failed = |stderr: &str| RegistryOutput {
            exit_code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
            interrupted: false,
        };
        let auth = DockerRegistry::new(FakeExecutor::new(vec![failed(
            "unauthorized: token=server-token top-secret",
        )]));
        let credential = RegistryCredential::Password {
            username: "robot".into(),
            secret: Secret::new("top-secret").unwrap(),
            credential_helper: "test-helper".into(),
        };
        let error = auth
            .login("registry.test", &credential, &NeverCancelled)
            .unwrap_err();
        assert_eq!(error.kind(), RegistryErrorKind::Authentication);
        assert!(!error.to_string().contains("server-token"));
        assert!(!error.to_string().contains("top-secret"));

        let network = DockerRegistry::new(FakeExecutor::new(vec![failed(
            "dial tcp: connection refused",
        )]));
        let error = network
            .login("registry.test", &credential, &NeverCancelled)
            .unwrap_err();
        assert_eq!(error.kind(), RegistryErrorKind::Network);
    }

    #[test]
    fn publish_creates_content_tag_then_alias_and_verifies_multiarch_digests() {
        let executor = FakeExecutor::new(vec![
            ok(""),
            ok(described()),
            ok(multiarch()),
            ok(""),
            ok(described()),
            ok(multiarch()),
        ]);
        let registry = DockerRegistry::new(executor);
        let request = PublishRequest {
            source: ImageRef::new("registry.test/source/image:build").unwrap(),
            repository: RegistryRepository::new("registry.test/team/image").unwrap(),
            digest: root_digest(),
            platform_digests: platform_digests(),
            aliases: vec![RegistryTag::new("latest").unwrap()],
        };
        let report = registry.publish(&request, &NeverCancelled).unwrap();
        assert_eq!(report.digest, root_digest());
        assert_eq!(report.platform_digests.len(), 2);
        assert!(
            report
                .immutable
                .as_str()
                .ends_with(&format!(":sha256-{}", &root_digest().as_str()[7..]))
        );
        assert_eq!(
            report.aliases[0].as_str(),
            "registry.test/team/image:latest"
        );
        let calls = registry.executor.invocations.lock().unwrap();
        assert_eq!(calls[0].0.args[0..3], ["buildx", "imagetools", "create"]);
        assert!(
            calls[0]
                .0
                .args
                .iter()
                .any(|arg| arg == &format!("registry.test/source/image:build@{}", root_digest()))
        );
    }

    #[test]
    fn single_manifest_publication_progress_retains_verified_immutable_on_alias_failure() {
        let executor = FakeExecutor::new(vec![
            ok(""),
            ok(described()),
            ok(single_manifest()),
            failed("alias copy denied"),
        ]);
        let registry = DockerRegistry::new(executor);
        let request = PublishRequest {
            source: ImageRef::new("registry.test/source/image:build").unwrap(),
            repository: RegistryRepository::new("registry.test/team/image").unwrap(),
            digest: root_digest(),
            platform_digests: vec![PlatformDigest {
                platform: Platform::LinuxAmd64,
                digest: root_digest(),
            }],
            aliases: vec![RegistryTag::new("latest").unwrap()],
        };
        let mut observed = Vec::new();
        assert!(
            registry
                .publish_with_progress(&request, &NeverCancelled, |state| observed
                    .push(state.clone()))
                .is_err()
        );
        assert_eq!(observed.len(), 2);
        assert_eq!(observed.last().unwrap().digest, root_digest());
        assert!(observed.last().unwrap().aliases.is_empty());
    }

    #[test]
    fn multi_manifest_publication_progress_retains_each_verified_alias_before_later_failure() {
        let executor = FakeExecutor::new(vec![
            ok(""),
            ok(described()),
            ok(multiarch()),
            ok(""),
            ok(described()),
            ok(multiarch()),
            ok(""),
            failed("alias inspect transport failure"),
        ]);
        let registry = DockerRegistry::new(executor);
        let request = PublishRequest {
            source: ImageRef::new("registry.test/source/image:build").unwrap(),
            repository: RegistryRepository::new("registry.test/team/image").unwrap(),
            digest: root_digest(),
            platform_digests: platform_digests(),
            aliases: vec![
                RegistryTag::new("stable").unwrap(),
                RegistryTag::new("latest").unwrap(),
            ],
        };
        let mut observed = Vec::new();
        assert!(
            registry
                .publish_with_progress(&request, &NeverCancelled, |state| observed
                    .push(state.clone()))
                .is_err()
        );
        assert_eq!(observed.len(), 5);
        assert!(observed[0].aliases.is_empty());
        assert_eq!(observed[3].aliases.len(), 1);
        assert!(observed[3].aliases[0].as_str().ends_with(":stable"));
        assert_eq!(observed.last().unwrap().aliases.len(), 2);
        assert!(
            observed.last().unwrap().aliases[1]
                .as_str()
                .ends_with(":latest")
        );
        assert_eq!(
            observed.last().unwrap().platform_digests,
            platform_digests()
        );
    }

    #[test]
    fn immutable_copy_fact_is_reported_even_when_its_verification_transport_fails() {
        let executor = FakeExecutor::new(vec![ok(""), failed("inspect timed out")]);
        let registry = DockerRegistry::new(executor);
        let request = PublishRequest {
            source: ImageRef::new("registry.test/source/image:build").unwrap(),
            repository: RegistryRepository::new("registry.test/team/image").unwrap(),
            digest: root_digest(),
            platform_digests: platform_digests(),
            aliases: Vec::new(),
        };
        let mut observed = Vec::new();
        assert!(
            registry
                .publish_with_progress(&request, &NeverCancelled, |state| observed
                    .push(state.clone()))
                .is_err()
        );
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].digest, request.digest);
        assert_eq!(observed[0].platform_digests, request.platform_digests);
    }

    #[test]
    fn pull_fetches_every_platform_by_pinned_digest_and_rechecks_tag() {
        let executor = FakeExecutor::new(vec![
            ok(described()),
            ok(multiarch()),
            ok(""),
            ok("linux/amd64\n"),
            ok(""),
            ok("linux/arm64\n"),
            ok(described()),
            ok(multiarch()),
        ]);
        let registry = DockerRegistry::new(executor);
        let request = PullRequest {
            image: ImageRef::new("registry.test/team/image:latest").unwrap(),
            expected_digest: root_digest(),
            expected_platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
        };
        let report = registry.pull_and_verify(&request, &NeverCancelled).unwrap();
        assert_eq!(report.platforms.len(), 2);
        let calls = registry.executor.invocations.lock().unwrap();
        let pulls: Vec<_> = calls
            .iter()
            .filter(|(call, _)| call.args.first().is_some_and(|arg| arg == "pull"))
            .collect();
        assert_eq!(pulls.len(), 2);
        assert!(
            pulls.iter().all(|(call, _)| call
                .args
                .last()
                .unwrap()
                .ends_with(root_digest().as_str()))
        );
    }

    #[test]
    fn manifest_parser_rejects_missing_or_changed_platforms() {
        let actual = parse_platforms(&multiarch()).unwrap();
        verify_platforms(&actual, &platform_digests()).unwrap();
        let changed = vec![PlatformDigest {
            platform: Platform::LinuxAmd64,
            digest: root_digest(),
        }];
        assert_eq!(
            verify_platforms(&actual, &changed).unwrap_err().kind(),
            RegistryErrorKind::DigestMismatch
        );
    }

    #[test]
    fn descriptor_digest_is_not_inferred_from_raw_stdout_bytes() {
        let raw_with_cli_newline = multiarch();
        assert!(raw_with_cli_newline.ends_with('\n'));
        assert_eq!(
            parse_descriptor_digest(&described()).unwrap(),
            root_digest()
        );
        assert_ne!(
            format!(
                "sha256:{:x}",
                sha2::Sha256::digest(raw_with_cli_newline.as_bytes())
            ),
            ROOT
        );
    }

    #[test]
    fn single_manifest_requires_one_platform_and_verifies_the_pulled_image() {
        let image = ImageRef::new("registry.test/team/image:single").unwrap();
        let rejected = DockerRegistry::new(FakeExecutor::new(vec![
            ok(described()),
            ok(single_manifest()),
        ]));
        let error = rejected
            .pull_and_verify(
                &PullRequest {
                    image: image.clone(),
                    expected_digest: root_digest(),
                    expected_platforms: vec![Platform::LinuxAmd64, Platform::LinuxArm64],
                },
                &NeverCancelled,
            )
            .unwrap_err();
        assert_eq!(error.kind(), RegistryErrorKind::Manifest);

        let accepted = DockerRegistry::new(FakeExecutor::new(vec![
            ok(described()),
            ok(single_manifest()),
            ok(""),
            ok("linux/amd64\n"),
            ok(described()),
            ok(single_manifest()),
        ]));
        accepted
            .pull_and_verify(
                &PullRequest {
                    image,
                    expected_digest: root_digest(),
                    expected_platforms: vec![Platform::LinuxAmd64],
                },
                &NeverCancelled,
            )
            .unwrap();
    }

    #[test]
    fn publication_accepts_an_exact_single_platform_manifest() {
        let executor = FakeExecutor::new(vec![ok(""), ok(described()), ok(single_manifest())]);
        let registry = DockerRegistry::new(executor);
        let request = PublishRequest {
            source: ImageRef::new("registry.test/team/image:source").unwrap(),
            repository: RegistryRepository::new("registry.test/team/image").unwrap(),
            digest: root_digest(),
            platform_digests: vec![PlatformDigest {
                platform: Platform::LinuxAmd64,
                digest: root_digest(),
            }],
            aliases: Vec::new(),
        };
        let published = registry.publish(&request, &NeverCancelled).unwrap();
        assert_eq!(published.platform_digests, request.platform_digests);
    }

    /// Configurable end-to-end coverage against an operator-provided disposable repository.
    /// It deliberately neither provisions nor deletes a registry or its credentials.
    #[test]
    #[ignore = "requires Docker buildx, configured credential helper, and REPO_SANDBOX_REGISTRY_TEST_SOURCE/REPOSITORY"]
    fn docker_registry_login_publish_pull_and_multiarch_digest_consistency() {
        let source = ImageRef::new(
            std::env::var("REPO_SANDBOX_REGISTRY_TEST_SOURCE")
                .expect("REPO_SANDBOX_REGISTRY_TEST_SOURCE must be a readable multiarch image"),
        )
        .unwrap();
        let repository = RegistryRepository::new(
            std::env::var("REPO_SANDBOX_REGISTRY_TEST_REPOSITORY")
                .expect("REPO_SANDBOX_REGISTRY_TEST_REPOSITORY must be writable and disposable"),
        )
        .unwrap();
        let registry = DockerRegistry::new(SystemRegistryExecutor);
        registry
            .login(
                repository.registry(),
                &RegistryCredential::CredentialHelper {
                    access_probe: source.clone(),
                },
                &NeverCancelled,
            )
            .unwrap();
        let source_manifest = registry
            .inspect_digest(&source, &[], &NeverCancelled)
            .unwrap();
        assert!(
            source_manifest.platforms.len() >= 2,
            "integration source must contain a multi-platform OCI index"
        );
        let published = registry
            .publish(
                &PublishRequest {
                    source,
                    repository,
                    digest: source_manifest.digest.clone(),
                    platform_digests: source_manifest.platforms.clone(),
                    aliases: vec![RegistryTag::new("issue-11-integration").unwrap()],
                },
                &NeverCancelled,
            )
            .unwrap();
        let pulled = registry
            .pull_and_verify(
                &PullRequest {
                    image: published.immutable,
                    expected_digest: published.digest.clone(),
                    expected_platforms: published
                        .platform_digests
                        .iter()
                        .map(|item| item.platform)
                        .collect(),
                },
                &NeverCancelled,
            )
            .unwrap();
        assert_eq!(pulled.digest, published.digest);
    }
}
