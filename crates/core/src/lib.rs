//! Domain types and use-case boundaries for repo-sandbox.
//!
//! This crate deliberately has no infrastructure dependencies.

pub mod application;
pub mod build;
pub mod config;
pub mod doctor;
pub mod exit_code;
pub mod registry;
pub mod runner;
pub mod snapshot;
pub mod task_image;
pub mod template;

use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// Commands understood by the application layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Doctor,
    Plan,
    Build,
    Verify,
    Clean,
}

impl Command {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Clean => "clean",
        }
    }
}

/// Stable, infrastructure-independent application error model.
#[derive(Debug, Eq, PartialEq)]
pub enum AppError {
    Configuration(String),
    Environment(String),
    BuildFailed(String),
    TestFailed(String),
}

impl AppError {
    pub const fn exit_code(&self) -> exit_code::ExitCode {
        match self {
            Self::Configuration(_) => exit_code::ExitCode::Configuration,
            Self::Environment(_) => exit_code::ExitCode::Environment,
            Self::BuildFailed(_) => exit_code::ExitCode::BuildFailed,
            Self::TestFailed(_) => exit_code::ExitCode::TestFailed,
        }
    }
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => write!(formatter, "configuration error: {message}"),
            Self::Environment(message) => write!(formatter, "environment error: {message}"),
            Self::BuildFailed(message) => write!(formatter, "build failed: {message}"),
            Self::TestFailed(message) => write!(formatter, "test failed: {message}"),
        }
    }
}

impl Error for AppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_errors_map_to_stable_exit_codes() {
        assert_eq!(
            AppError::Configuration("bad yaml".into()).exit_code(),
            exit_code::ExitCode::Configuration
        );
        assert_eq!(
            AppError::Environment("docker unavailable".into()).exit_code(),
            exit_code::ExitCode::Environment
        );
        assert_eq!(
            AppError::BuildFailed("compiler failed".into()).exit_code(),
            exit_code::ExitCode::BuildFailed
        );
        assert_eq!(
            AppError::TestFailed("assertion failed".into()).exit_code(),
            exit_code::ExitCode::TestFailed
        );
    }
}
