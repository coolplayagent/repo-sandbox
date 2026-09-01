//! Domain types and use-case boundaries for repo-sandbox.
//!
//! This crate deliberately has no infrastructure dependencies.

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
    InvalidInput(String),
    Infrastructure(String),
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid input: {message}"),
            Self::Infrastructure(message) => write!(formatter, "infrastructure error: {message}"),
        }
    }
}

impl Error for AppError {}

/// Route a command to its future use case.
///
/// Issue #2 only reserves these routes; the implementations intentionally live
/// in later issues.
pub fn route(command: Command) -> Result<String, AppError> {
    Ok(format!("{} is not implemented yet", command.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reserved_command_has_a_route() {
        for command in [
            Command::Doctor,
            Command::Plan,
            Command::Build,
            Command::Verify,
            Command::Clean,
        ] {
            assert_eq!(
                route(command).unwrap(),
                format!("{} is not implemented yet", command.name())
            );
        }
    }
}
