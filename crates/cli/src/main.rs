use clap::Parser;
use repo_sandbox_cli::{Cli, run};

fn main() {
    repo_sandbox_adapters::logging::init();

    match run(Cli::parse()) {
        Ok(Some(message)) => println!("{message}"),
        Ok(None) => {}
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
