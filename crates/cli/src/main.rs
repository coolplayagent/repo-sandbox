use clap::Parser;
use repo_sandbox_cli::{Cli, run};

fn main() {
    repo_sandbox_adapters::logging::init();
    if let Err(error) = repo_sandbox_adapters::cancellation::install() {
        eprintln!("error: cannot install interrupt handler: {error}");
        std::process::exit(repo_sandbox_core::exit_code::ExitCode::Environment.as_i32());
    }

    match run(Cli::parse()) {
        Ok(output) => {
            if let Some(message) = output.message {
                println!("{message}");
            }
            if output.exit_code != repo_sandbox_core::exit_code::ExitCode::Success {
                std::process::exit(output.exit_code.as_i32());
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(error.exit_code().as_i32());
        }
    }
}
