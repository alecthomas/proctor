mod orchestrator;
mod output;
mod parser;
mod readiness;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "proctor", about = "A process manager with hot reload")]
struct Cli {
    /// Path to the Procfile
    #[arg(default_value = "Procfile")]
    procfile: PathBuf,

    /// Validate the Procfile without running processes
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let procfile = match load_procfile(&cli.procfile) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            return ExitCode::FAILURE;
        }
    };

    if cli.check {
        println!(
            "Procfile is valid ({} process{})",
            procfile.processes.len(),
            if procfile.processes.len() == 1 {
                ""
            } else {
                "es"
            }
        );
        return ExitCode::SUCCESS;
    }

    if let Err(e) = run(procfile) {
        eprintln!("error: {}", e);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn load_procfile(path: &PathBuf) -> Result<parser::Procfile, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;

    parser::parse(&content).map_err(|e| format!("{}: {}", path.display(), e))
}

fn run(procfile: parser::Procfile) -> Result<(), String> {
    let base_dir = std::env::current_dir().map_err(|e| e.to_string())?;
    let orchestrator = orchestrator::Orchestrator::new(procfile, base_dir);
    orchestrator.run().map_err(|e| e.to_string())
}
