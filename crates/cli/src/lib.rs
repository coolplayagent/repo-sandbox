use clap::{Parser, Subcommand};
use repo_sandbox_core::{AppError, Command, route};

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

#[derive(Clone, Copy, Debug, Subcommand)]
enum Commands {
    /// Inspect local prerequisites (reserved).
    Doctor,
    /// Produce an execution plan (reserved).
    Plan,
    /// Build sandbox artifacts (reserved).
    Build,
    /// Verify sandbox artifacts (reserved).
    Verify,
    /// Remove generated sandbox artifacts (reserved).
    Clean,
}

impl From<Commands> for Command {
    fn from(value: Commands) -> Self {
        match value {
            Commands::Doctor => Self::Doctor,
            Commands::Plan => Self::Plan,
            Commands::Build => Self::Build,
            Commands::Verify => Self::Verify,
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
}
