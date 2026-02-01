mod orchestrator;
mod output;
mod parser;
mod readiness;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

const SYNTAX_HELP: &str = "\
PROCFILE SYNTAX:
  <name>[!] [pattern...] [option=value...]: [ENV=VAR...] <command>

  name!         One-shot process (ready on exit 0)
  name          Long-running process (ready on start)
  **/*.go       Watch pattern (glob syntax, triggers reload)
  !vendor/**    Exclusion pattern

OPTIONS:
  after=name[,name2]   Wait for dependencies before starting
  ready=PORT           Readiness probe (<port> for TCP, http:<port>[/<path>] for HTTP)
  signal=TERM          Reload signal (HUP, INT, TERM, KILL, USR1, USR2)
  debounce=500ms       File watch debounce interval
  dir=./path           Working directory
  shutdown=5s          Grace period before SIGKILL

EXAMPLE:
  migrate! after=db: just db migrate
  api **/*.go !**_test.go after=migrate ready=8080: go run ./cmd/api";

#[derive(Parser)]
#[command(name = "proctor", about = "A process manager with hot reload", after_help = SYNTAX_HELP)]
struct Cli {
    /// Path to the Procfile
    #[arg(default_value = "Procfile")]
    procfile: PathBuf,

    /// Validate the Procfile without running processes
    #[arg(long)]
    check: bool,

    /// Print commands being executed
    #[arg(short = 'd', long)]
    debug: bool,

    /// Print elapsed time since start for each line
    #[arg(short = 't', long)]
    timestamp: bool,
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
            if procfile.processes.len() == 1 { "" } else { "es" }
        );
        return ExitCode::SUCCESS;
    }

    if let Err(e) = run(procfile, cli.debug, cli.timestamp) {
        eprintln!("error: {}", e);
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn load_procfile(path: &PathBuf) -> Result<parser::Procfile, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;

    parser::parse(&content).map_err(|e| format!("{}: {}", path.display(), e))
}

fn run(procfile: parser::Procfile, debug: bool, timestamp: bool) -> Result<(), String> {
    let base_dir = std::env::current_dir().map_err(|e| e.to_string())?;
    let orchestrator = orchestrator::Orchestrator::new(procfile, base_dir, debug, timestamp);
    orchestrator.run().map_err(|e| e.to_string())
}
