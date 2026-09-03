use clap::Parser;
use repo_sandbox_cli::{Cli, run};

fn main() {
    repo_sandbox_adapters::logging::init();

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
