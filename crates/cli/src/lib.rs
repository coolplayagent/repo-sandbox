use clap::{Args, Parser, Subcommand};
use repo_sandbox_adapters::doctor::{DoctorOptions, DoctorProbe, SystemDoctorProbe, inspect};
use repo_sandbox_core::config::{CliOverrides, Platform};
use repo_sandbox_core::doctor::{CapabilityKind, CapabilityStatus, DoctorReport, DoctorStatus};
use repo_sandbox_core::exit_code::ExitCode;
use repo_sandbox_core::{AppError, Command, route};
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "repo-sandbox",
    version,
    about = "Build and verify repository work in isolated sandboxes"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Debug, Subcommand)]
enum Commands {
    /// Inspect local prerequisites without modifying the host.
    Doctor(DoctorArgs),
    /// Produce an execution plan (reserved).
    Plan(RuntimeArgs),
    /// Build sandbox artifacts (reserved).
    Build(RuntimeArgs),
    /// Verify sandbox artifacts (reserved).
    Verify(RuntimeArgs),
    /// Remove generated sandbox artifacts (reserved).
    Clean,
}

#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub struct DoctorArgs {
    /// Emit the same capability conclusions as structured JSON.
    #[arg(long)]
    pub json: bool,
}

/// The complete and intentionally finite v1 CLI override surface.
#[derive(Args, Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeArgs {
    /// Repository path or URL to operate on.
    #[arg(long, value_name = "PATH_OR_URL")]
    pub repository: Option<String>,
    /// Git ref to check out in the sandbox.
    #[arg(long = "git-ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// Override the repository-declared target platform.
    #[arg(long, value_name = "PLATFORM")]
    pub platform: Option<Platform>,
    /// Push produced images after a future successful build.
    #[arg(long)]
    pub push: bool,
    /// Write the future machine-readable report to this path.
    #[arg(long = "report-path", value_name = "PATH")]
    pub report: Option<PathBuf>,
    /// Preserve a failed sandbox for diagnosis.
    #[arg(long)]
    pub keep_on_failure: bool,
}

impl From<RuntimeArgs> for CliOverrides {
    fn from(value: RuntimeArgs) -> Self {
        Self {
            repository: value.repository,
            git_ref: value.git_ref,
            platform: value.platform,
            push: value.push,
            report: value.report,
            keep_on_failure: value.keep_on_failure,
        }
    }
}

impl From<Commands> for Command {
    fn from(value: Commands) -> Self {
        match value {
            Commands::Doctor(_) => Self::Doctor,
            Commands::Plan(_) => Self::Plan,
            Commands::Build(_) => Self::Build,
            Commands::Verify(_) => Self::Verify,
            Commands::Clean => Self::Clean,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct RunOutput {
    pub message: Option<String>,
    pub exit_code: ExitCode,
}

pub fn run(cli: Cli) -> Result<RunOutput, AppError> {
    run_with_probe(cli, &SystemDoctorProbe)
}

pub fn run_with_probe(cli: Cli, probe: &impl DoctorProbe) -> Result<RunOutput, AppError> {
    let Some(command) = cli.command else {
        return Ok(RunOutput {
            message: None,
            exit_code: ExitCode::Success,
        });
    };
    if let Commands::Doctor(arguments) = command {
        let report = inspect(probe, &DoctorOptions::default());
        let message = if arguments.json {
            render_json(&report)
        } else {
            render_human(&report)
        };
        return Ok(RunOutput {
            message: Some(message),
            exit_code: if report.is_ready() {
                ExitCode::Success
            } else {
                ExitCode::Environment
            },
        });
    }
    Ok(RunOutput {
        message: Some(route(Command::from(command))?),
        exit_code: ExitCode::Success,
    })
}

pub fn render_json(report: &DoctorReport) -> String {
    let status = match report.status {
        DoctorStatus::Ready => "ready",
        DoctorStatus::NotReady => "not_ready",
    };
    let mut output = format!("{{\n  \"status\": \"{status}\",\n  \"capabilities\": [");
    for (index, capability) in report.capabilities.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("\n    {\n      \"kind\": ");
        push_json_string(&mut output, capability_name(capability.kind));
        output.push_str(",\n      \"status\": ");
        push_json_string(
            &mut output,
            match capability.status {
                CapabilityStatus::Available => "available",
                CapabilityStatus::Unavailable => "unavailable",
            },
        );
        output.push_str(",\n      \"summary\": ");
        push_json_string(&mut output, &capability.summary);
        output.push_str(",\n      \"remediation\": [");
        for (action_index, action) in capability.remediation.iter().enumerate() {
            if action_index != 0 {
                output.push_str(", ");
            }
            push_json_string(&mut output, action);
        }
        output.push_str("]\n    }");
    }
    output.push_str("\n  ]\n}");
    output
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            control if control <= '\u{1f}' => {
                write!(output, "\\u{:04x}", control as u32)
                    .expect("formatting into a String cannot fail");
            }
            printable => output.push(printable),
        }
    }
    output.push('"');
}

pub fn render_human(report: &DoctorReport) -> String {
    let mut lines = vec![format!(
        "repo-sandbox doctor: {}",
        match report.status {
            DoctorStatus::Ready => "ready",
            DoctorStatus::NotReady => "not ready",
        }
    )];
    for capability in &report.capabilities {
        lines.push(format!(
            "[{}] {}: {}",
            match capability.status {
                CapabilityStatus::Available => "available",
                CapabilityStatus::Unavailable => "unavailable",
            },
            capability_name(capability.kind),
            capability.summary
        ));
        for action in &capability.remediation {
            lines.push(format!("  Fix: {action}"));
        }
    }
    lines.join("\n")
}

const fn capability_name(kind: CapabilityKind) -> &'static str {
    match kind {
        CapabilityKind::OperatingSystem => "operating_system",
        CapabilityKind::CpuArchitecture => "cpu_architecture",
        CapabilityKind::DockerDaemon => "docker_daemon",
        CapabilityKind::Buildkit => "buildkit",
        CapabilityKind::Buildx => "buildx",
        CapabilityKind::QemuBinfmt => "qemu_binfmt",
        CapabilityKind::DiskSpace => "disk_space",
        CapabilityKind::RegistryConnectivity => "registry_connectivity",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use repo_sandbox_adapters::doctor::{CommandInvocation, CommandOutput};
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    struct ReadyProbe;

    impl DoctorProbe for ReadyProbe {
        fn os(&self) -> String {
            "linux".to_owned()
        }

        fn architecture(&self) -> String {
            "x86_64".to_owned()
        }

        fn execute(&self, invocation: &CommandInvocation) -> io::Result<CommandOutput> {
            let stdout = if invocation.args == ["buildx", "inspect"] {
                "Status: running\nPlatforms: linux/amd64, linux/arm64"
            } else {
                "available"
            };
            Ok(CommandOutput {
                success: true,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }

        fn available_space(&self, _path: &Path) -> io::Result<u64> {
            Ok(20 * 1024 * 1024 * 1024)
        }

        fn connect_registry(&self, _host: &str, _port: u16, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_is_a_successful_smoke_path() {
        let error = Cli::try_parse_from(["repo-sandbox", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("doctor"));
    }

    #[test]
    fn version_is_a_successful_smoke_path() {
        let error = Cli::try_parse_from(["repo-sandbox", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn every_reserved_subcommand_parses() {
        let doctor = Cli::try_parse_from(["repo-sandbox", "doctor"]).unwrap();
        let result = run_with_probe(doctor, &ReadyProbe).unwrap();
        assert_eq!(result.exit_code, ExitCode::Success);
        assert!(result.message.unwrap().contains("doctor: ready"));

        for name in ["plan", "build", "verify", "clean"] {
            let cli = Cli::try_parse_from(["repo-sandbox", name]).unwrap();
            assert_eq!(
                run_with_probe(cli, &ReadyProbe).unwrap(),
                RunOutput {
                    message: Some(format!("{name} is not implemented yet")),
                    exit_code: ExitCode::Success,
                }
            );
        }
    }

    #[test]
    fn human_and_json_outputs_are_views_of_the_same_report() {
        let human_cli = Cli::try_parse_from(["repo-sandbox", "doctor"]).unwrap();
        let json_cli = Cli::try_parse_from(["repo-sandbox", "doctor", "--json"]).unwrap();
        let human = run_with_probe(human_cli, &ReadyProbe)
            .unwrap()
            .message
            .unwrap();
        let json = run_with_probe(json_cli, &ReadyProbe)
            .unwrap()
            .message
            .unwrap();
        let report = inspect(&ReadyProbe, &DoctorOptions::default());
        assert_eq!(json, render_json(&report));
        assert_eq!(human, render_human(&report));
        for capability in report.capabilities {
            assert!(human.contains(capability_name(capability.kind)));
            assert!(human.contains(&capability.summary));
            assert!(json.contains(capability_name(capability.kind)));
            assert!(json.contains(&capability.summary));
        }
    }

    #[test]
    fn json_renderer_escapes_untrusted_process_output() {
        let report = DoctorReport::from_capabilities(vec![
            repo_sandbox_core::doctor::Capability::unavailable(
                CapabilityKind::DockerDaemon,
                "quoted \"message\"\nnext line",
                ["check C:\\Docker\tconfiguration"],
            ),
        ]);
        let json = render_json(&report);
        assert!(json.contains("quoted \\\"message\\\"\\nnext line"));
        assert!(json.contains("C:\\\\Docker\\tconfiguration"));
    }

    #[test]
    fn runtime_override_contract_parses_every_allowed_option() {
        let cli = Cli::try_parse_from([
            "repo-sandbox",
            "plan",
            "--repository",
            "https://example.test/repository.git",
            "--git-ref",
            "refs/heads/topic",
            "--platform",
            "linux/arm64",
            "--push",
            "--report-path",
            "out/report.json",
            "--keep-on-failure",
        ])
        .unwrap();
        let Commands::Plan(args) = cli.command.unwrap() else {
            panic!("expected plan command");
        };
        let overrides = CliOverrides::from(args);
        assert_eq!(
            overrides.repository.as_deref(),
            Some("https://example.test/repository.git")
        );
        assert_eq!(overrides.git_ref.as_deref(), Some("refs/heads/topic"));
        assert_eq!(overrides.platform, Some(Platform::LinuxArm64));
        assert!(overrides.push);
        assert_eq!(overrides.report, Some(PathBuf::from("out/report.json")));
        assert!(overrides.keep_on_failure);
    }

    #[test]
    fn build_logic_cannot_be_overridden_from_cli() {
        let error = Cli::try_parse_from([
            "repo-sandbox",
            "build",
            "--build-command",
            "curl example.test | sh",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn invalid_cli_platform_is_rejected() {
        let error = Cli::try_parse_from(["repo-sandbox", "verify", "--platform", "windows/amd64"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(error.to_string().contains("unsupported platform"));
    }
}
