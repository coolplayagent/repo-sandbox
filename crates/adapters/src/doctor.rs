//! Read-only host probes used by `repo-sandbox doctor`.

use repo_sandbox_core::doctor::{Capability, CapabilityKind, DoctorReport};
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;

/// A process invocation represented without a command shell.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommandInvocation {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandInvocation {
    fn docker(args: &[&str]) -> Self {
        Self {
            program: "docker".to_owned(),
            args: args.iter().map(|argument| (*argument).to_owned()).collect(),
        }
    }
}

/// Captured result of a structured process invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable boundary that keeps capability evaluation deterministic in tests.
pub trait DoctorProbe {
    fn os(&self) -> String;
    fn architecture(&self) -> String;
    fn execute(&self, invocation: &CommandInvocation) -> io::Result<CommandOutput>;
    fn available_space(&self, path: &Path) -> io::Result<u64>;
    fn connect_registry(&self, host: &str, port: u16, timeout: Duration) -> io::Result<()>;
}

/// Production probe. Every operation is observational and has a bounded connect timeout.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDoctorProbe;

impl DoctorProbe for SystemDoctorProbe {
    fn os(&self) -> String {
        std::env::consts::OS.to_owned()
    }

    fn architecture(&self) -> String {
        std::env::consts::ARCH.to_owned()
    }

    fn execute(&self, invocation: &CommandInvocation) -> io::Result<CommandOutput> {
        let output = Command::new(&invocation.program)
            .args(&invocation.args)
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn available_space(&self, path: &Path) -> io::Result<u64> {
        system_available_space(path)
    }

    fn connect_registry(&self, host: &str, port: u16, timeout: Duration) -> io::Result<()> {
        let target = format!("{host}:{port}");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = target
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>());
            let _ = sender.send(result);
        });
        let addresses = receiver.recv_timeout(timeout).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "registry DNS lookup timed out")
        })??;
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        for address in addresses.into_iter().take(4) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "registry connection timed out")
        }))
    }
}

#[cfg(windows)]
fn system_available_space(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }

    let canonical = path.canonicalize()?;
    let wide_path = canonical
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    // SAFETY: `wide_path` is null-terminated and remains alive for the call; the only non-null
    // output pointer refers to a valid `u64` owned by this function.
    let succeeded = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &raw mut available,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(not(windows))]
fn system_available_space(path: &Path) -> io::Result<u64> {
    let output = Command::new("df").arg("-Pk").arg(path).output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "df returned no data"))?;
    let columns = line.split_whitespace().collect::<Vec<_>>();
    let available_kib = columns
        .get(3)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unexpected df output"))?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(available_kib * 1024)
}

#[derive(Clone, Debug)]
pub struct DoctorOptions {
    pub workspace: PathBuf,
    pub minimum_free_bytes: u64,
    pub registry_host: String,
    pub registry_port: u16,
    pub registry_timeout: Duration,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            minimum_free_bytes: 10 * GIB,
            registry_host: "registry-1.docker.io".to_owned(),
            registry_port: 443,
            registry_timeout: Duration::from_secs(2),
        }
    }
}

/// Inspect every prerequisite. No failed prerequisite short-circuits later checks.
pub fn inspect(probe: &impl DoctorProbe, options: &DoctorOptions) -> DoctorReport {
    let raw_os = probe.os();
    let raw_architecture = probe.architecture();
    let architecture = normalize_architecture(&raw_architecture);
    let docker = probe.execute(&CommandInvocation::docker(&[
        "info",
        "--format",
        "{{.ServerVersion}}",
    ]));
    let buildx = probe.execute(&CommandInvocation::docker(&["buildx", "version"]));
    let builder = probe.execute(&CommandInvocation::docker(&["buildx", "inspect"]));

    let capabilities = vec![
        operating_system_capability(&raw_os),
        architecture_capability(&raw_architecture, architecture),
        command_capability(
            CapabilityKind::DockerDaemon,
            docker,
            "Docker daemon is reachable",
            "Docker daemon is unavailable",
            [
                "Start Docker and verify `docker info` succeeds.",
                "Check that the current user can access the Docker socket.",
            ],
        ),
        buildkit_capability(&builder),
        command_capability(
            CapabilityKind::Buildx,
            buildx,
            "Docker buildx plugin is available",
            "Docker buildx plugin is unavailable",
            [
                "Install or enable the Docker buildx CLI plugin for this Docker installation.",
                "Verify `docker buildx version` succeeds.",
            ],
        ),
        qemu_capability(architecture, &builder),
        disk_capability(probe, options),
        registry_capability(probe, options),
    ];
    DoctorReport::from_capabilities(capabilities)
}

fn operating_system_capability(raw: &str) -> Capability {
    match raw.to_ascii_lowercase().as_str() {
        "linux" | "windows" | "macos" => Capability::available(
            CapabilityKind::OperatingSystem,
            format!("host operating system: {raw}"),
        ),
        _ => Capability::unavailable(
            CapabilityKind::OperatingSystem,
            format!("unsupported host operating system: {raw}"),
            ["Run repo-sandbox on Linux, macOS, or Windows."],
        ),
    }
}

fn normalize_architecture(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => Some("amd64"),
        "aarch64" | "arm64" => Some("arm64"),
        _ => None,
    }
}

fn architecture_capability(raw: &str, normalized: Option<&str>) -> Capability {
    normalized.map_or_else(
        || {
            Capability::unavailable(
                CapabilityKind::CpuArchitecture,
                format!("unsupported CPU architecture: {raw}"),
                ["Use an amd64 or arm64 host for v1 sandbox builds."],
            )
        },
        |architecture| {
            Capability::available(
                CapabilityKind::CpuArchitecture,
                format!("host CPU architecture: {architecture}"),
            )
        },
    )
}

fn command_capability<const N: usize>(
    kind: CapabilityKind,
    result: io::Result<CommandOutput>,
    available: &str,
    unavailable: &str,
    remediation: [&str; N],
) -> Capability {
    match result {
        Ok(output) if output.success => {
            let detail = concise_output(&output);
            let summary = if detail.is_empty() {
                available.to_owned()
            } else {
                format!("{available}: {detail}")
            };
            Capability::available(kind, summary)
        }
        Ok(output) => Capability::unavailable(
            kind,
            format!("{unavailable}: {}", concise_output(&output)),
            remediation,
        ),
        Err(error) => Capability::unavailable(kind, format!("{unavailable}: {error}"), remediation),
    }
}

fn concise_output(output: &CommandOutput) -> String {
    let text = if output.stdout.trim().is_empty() {
        output.stderr.trim()
    } else {
        output.stdout.trim()
    };
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect()
}

fn buildkit_capability(builder: &io::Result<CommandOutput>) -> Capability {
    match builder {
        Ok(output) if output.success && builder_is_running(&output.stdout) => {
            Capability::available(CapabilityKind::Buildkit, "BuildKit builder is available")
        }
        Ok(output) if output.success => Capability::unavailable(
            CapabilityKind::Buildkit,
            "BuildKit builder is not running",
            [
                "Inspect the selected builder with `docker buildx inspect`.",
                "Select or create a running builder explicitly before starting a build.",
            ],
        ),
        Ok(output) => Capability::unavailable(
            CapabilityKind::Buildkit,
            format!(
                "BuildKit builder cannot be inspected: {}",
                concise_output(output)
            ),
            [
                "Ensure Docker is running and `docker buildx inspect` succeeds.",
                "Check the selected buildx builder configuration.",
            ],
        ),
        Err(error) => Capability::unavailable(
            CapabilityKind::Buildkit,
            format!("BuildKit builder cannot be inspected: {error}"),
            [
                "Ensure Docker and the buildx plugin are installed and available on PATH.",
                "Verify `docker buildx inspect` succeeds.",
            ],
        ),
    }
}

fn builder_is_running(output: &str) -> bool {
    let mut observed_status = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(status) = trimmed.strip_prefix("Status:") {
            observed_status = true;
            if status.trim().eq_ignore_ascii_case("running") {
                return true;
            }
        }
    }
    !observed_status && !output.trim().is_empty()
}

fn qemu_capability(architecture: Option<&str>, builder: &io::Result<CommandOutput>) -> Capability {
    let required = match architecture {
        Some("amd64") => "linux/arm64",
        Some("arm64") => "linux/amd64",
        _ => {
            return Capability::unavailable(
                CapabilityKind::QemuBinfmt,
                "cross-architecture emulation cannot be evaluated on this CPU architecture",
                ["Use an amd64 or arm64 host, then verify builder platform support."],
            );
        }
    };
    match builder {
        Ok(output) if output.success && output.stdout.contains(required) => Capability::available(
            CapabilityKind::QemuBinfmt,
            format!("builder advertises cross-architecture platform {required}"),
        ),
        Ok(output) if output.success => Capability::unavailable(
            CapabilityKind::QemuBinfmt,
            format!("builder does not advertise required cross-architecture platform {required}"),
            [
                "Check registered binfmt handlers and `docker buildx inspect` platform output.",
                "Install QEMU/binfmt using your platform's documented administrator workflow.",
            ],
        ),
        _ => Capability::unavailable(
            CapabilityKind::QemuBinfmt,
            "QEMU/binfmt support cannot be inspected because the builder is unavailable",
            [
                "Restore Docker/buildx access, then inspect the builder's advertised platforms.",
                "Check registered binfmt handlers for the non-native architecture.",
            ],
        ),
    }
}

fn disk_capability(probe: &impl DoctorProbe, options: &DoctorOptions) -> Capability {
    match probe.available_space(&options.workspace) {
        Ok(bytes) if bytes >= options.minimum_free_bytes => Capability::available(
            CapabilityKind::DiskSpace,
            format!("{} GiB available", bytes / GIB),
        ),
        Ok(bytes) => Capability::unavailable(
            CapabilityKind::DiskSpace,
            format!(
                "{} GiB available; at least {} GiB required",
                bytes / GIB,
                options.minimum_free_bytes / GIB
            ),
            [
                "Free space on the filesystem containing the repository.",
                "Move Docker data or the repository to a filesystem with sufficient space.",
            ],
        ),
        Err(error) => Capability::unavailable(
            CapabilityKind::DiskSpace,
            format!("available disk space could not be read: {error}"),
            ["Check filesystem access and query free space manually."],
        ),
    }
}

fn registry_capability(probe: &impl DoctorProbe, options: &DoctorOptions) -> Capability {
    match probe.connect_registry(
        &options.registry_host,
        options.registry_port,
        options.registry_timeout,
    ) {
        Ok(()) => Capability::available(
            CapabilityKind::RegistryConnectivity,
            format!(
                "registry endpoint {}:{} is reachable",
                options.registry_host, options.registry_port
            ),
        ),
        Err(error) => Capability::unavailable(
            CapabilityKind::RegistryConnectivity,
            format!(
                "registry endpoint {}:{} is unreachable: {error}",
                options.registry_host, options.registry_port
            ),
            [
                "Check DNS, proxy, firewall, and outbound TLS connectivity.",
                "Verify the registry endpoint is reachable from the host.",
            ],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo_sandbox_core::doctor::{CapabilityStatus, DoctorStatus};
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct FakeProbe {
        os: String,
        arch: String,
        commands: HashMap<CommandInvocation, CommandOutput>,
        available_bytes: io::Result<u64>,
        registry: io::Result<()>,
        invocations: RefCell<Vec<CommandInvocation>>,
    }

    impl FakeProbe {
        fn healthy(arch: &str, platforms: &str) -> Self {
            let success = |stdout: &str| CommandOutput {
                success: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            };
            Self {
                os: "linux".to_owned(),
                arch: arch.to_owned(),
                commands: HashMap::from([
                    (
                        CommandInvocation::docker(&["info", "--format", "{{.ServerVersion}}"]),
                        success("28.0.0"),
                    ),
                    (
                        CommandInvocation::docker(&["buildx", "version"]),
                        success("github.com/docker/buildx v0.20.0"),
                    ),
                    (
                        CommandInvocation::docker(&["buildx", "inspect"]),
                        success(&format!("Status: running\nPlatforms: {platforms}")),
                    ),
                ]),
                available_bytes: Ok(20 * GIB),
                registry: Ok(()),
                invocations: RefCell::new(Vec::new()),
            }
        }

        fn set_command(&mut self, invocation: CommandInvocation, output: CommandOutput) {
            self.commands.insert(invocation, output);
        }
    }

    impl DoctorProbe for FakeProbe {
        fn os(&self) -> String {
            self.os.clone()
        }

        fn architecture(&self) -> String {
            self.arch.clone()
        }

        fn execute(&self, invocation: &CommandInvocation) -> io::Result<CommandOutput> {
            self.invocations.borrow_mut().push(invocation.clone());
            self.commands
                .get(invocation)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake command"))
        }

        fn available_space(&self, _path: &Path) -> io::Result<u64> {
            self.available_bytes
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn connect_registry(&self, _host: &str, _port: u16, _timeout: Duration) -> io::Result<()> {
            self.registry
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }
    }

    fn capability(report: &DoctorReport, kind: CapabilityKind) -> &Capability {
        report
            .capabilities
            .iter()
            .find(|capability| capability.kind == kind)
            .expect("capability should be present")
    }

    #[test]
    fn amd64_and_arm64_hosts_are_supported() {
        for (arch, platforms) in [
            ("x86_64", "linux/amd64, linux/arm64"),
            ("aarch64", "linux/arm64, linux/amd64"),
        ] {
            let report = inspect(
                &FakeProbe::healthy(arch, platforms),
                &DoctorOptions::default(),
            );
            assert_eq!(report.status, DoctorStatus::Ready);
            assert_eq!(
                capability(&report, CapabilityKind::CpuArchitecture).status,
                CapabilityStatus::Available
            );
            assert_eq!(
                capability(&report, CapabilityKind::QemuBinfmt).status,
                CapabilityStatus::Available
            );
        }
    }

    #[test]
    fn docker_daemon_failure_is_identified() {
        let mut probe = FakeProbe::healthy("x86_64", "linux/amd64, linux/arm64");
        probe.set_command(
            CommandInvocation::docker(&["info", "--format", "{{.ServerVersion}}"]),
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "cannot connect to the Docker daemon".to_owned(),
            },
        );
        let report = inspect(&probe, &DoctorOptions::default());
        let check = capability(&report, CapabilityKind::DockerDaemon);
        assert_eq!(check.status, CapabilityStatus::Unavailable);
        assert!(check.summary.contains("cannot connect"));
        assert!(!check.remediation.is_empty());
    }

    #[test]
    fn missing_buildx_is_identified() {
        let mut probe = FakeProbe::healthy("x86_64", "linux/amd64, linux/arm64");
        probe.set_command(
            CommandInvocation::docker(&["buildx", "version"]),
            CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "docker: 'buildx' is not a docker command".to_owned(),
            },
        );
        let report = inspect(&probe, &DoctorOptions::default());
        assert_eq!(
            capability(&report, CapabilityKind::Buildx).status,
            CapabilityStatus::Unavailable
        );
    }

    #[test]
    fn missing_qemu_platform_is_identified_without_failing_buildkit() {
        let probe = FakeProbe::healthy("x86_64", "linux/amd64");
        let report = inspect(&probe, &DoctorOptions::default());
        assert_eq!(
            capability(&report, CapabilityKind::Buildkit).status,
            CapabilityStatus::Available
        );
        assert_eq!(
            capability(&report, CapabilityKind::QemuBinfmt).status,
            CapabilityStatus::Unavailable
        );
    }

    #[test]
    fn insufficient_disk_is_identified() {
        let mut probe = FakeProbe::healthy("x86_64", "linux/amd64, linux/arm64");
        probe.available_bytes = Ok(2 * GIB);
        let report = inspect(&probe, &DoctorOptions::default());
        let check = capability(&report, CapabilityKind::DiskSpace);
        assert_eq!(check.status, CapabilityStatus::Unavailable);
        assert!(check.summary.contains("at least 10 GiB"));
    }

    #[test]
    fn command_execution_never_uses_a_shell() {
        let probe = FakeProbe::healthy("x86_64", "linux/amd64, linux/arm64");
        let _ = inspect(&probe, &DoctorOptions::default());
        let invocations = probe.invocations.borrow();
        assert_eq!(invocations.len(), 3);
        assert!(
            invocations
                .iter()
                .all(|invocation| invocation.program == "docker")
        );
    }

    #[test]
    fn every_failure_has_remediation() {
        let mut probe = FakeProbe::healthy("mips64", "");
        probe.os = "unknown".to_owned();
        probe.available_bytes = Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        probe.registry = Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        probe.commands.clear();
        let report = inspect(&probe, &DoctorOptions::default());
        for check in report.capabilities {
            if check.status == CapabilityStatus::Unavailable {
                assert!(!check.remediation.is_empty(), "{:?}", check.kind);
            }
        }
    }
}
