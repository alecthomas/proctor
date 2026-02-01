pub mod runner;

use crate::output::OutputFormatter;
use crate::parser::{Procfile, ReadyProbe};
use crate::readiness;
use runner::{ProcessOutput, RunningProcess, spawn_process};
use std::collections::HashSet;
use std::io;
use std::thread;
use std::time::Duration;

pub struct Orchestrator {
    procfile: Procfile,
    base_dir: std::path::PathBuf,
}

struct ManagedProcess {
    name: String,
    process: RunningProcess,
    output: ProcessOutput,
    ready_probe: Option<ReadyProbe>,
    is_ready: bool,
    is_oneshot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Success,
    Failed(i32),
    Signaled(i32),
    Unknown,
}

impl std::fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessStatus::Success => write!(f, "Exited successfully"),
            ProcessStatus::Failed(code) => write!(f, "Exited with code {}", code),
            ProcessStatus::Signaled(sig) => write!(f, "Killed by signal {}", sig),
            ProcessStatus::Unknown => write!(f, "Exited with unknown status"),
        }
    }
}

impl Orchestrator {
    pub fn new(procfile: Procfile, base_dir: std::path::PathBuf) -> Self {
        Self { procfile, base_dir }
    }

    pub fn run(&self) -> io::Result<()> {
        if self.procfile.processes.is_empty() {
            return Ok(());
        }

        let process_names: Vec<&str> = self
            .procfile
            .processes
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let formatter = OutputFormatter::new(&process_names);

        let mut processes: Vec<ManagedProcess> = Vec::new();

        // Spawn all processes
        for def in &self.procfile.processes {
            let mut running = spawn_process(def, &self.base_dir, None)?;
            let output = running.take_output().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "failed to capture process output")
            })?;

            let is_oneshot = def.oneshot;
            let ready_probe = def.options.ready.clone();

            // If no probe and long-running (not oneshot), it's ready immediately
            let is_ready = ready_probe.is_none() && !is_oneshot;

            processes.push(ManagedProcess {
                name: def.name.clone(),
                process: running,
                output,
                ready_probe,
                is_ready,
                is_oneshot,
            });

            if is_ready {
                let msg = formatter.format_control(&def.name, "Ready (started)");
                println!("{}", msg);
            }
        }

        let poll_interval = Duration::from_millis(10);
        let mut exited: HashSet<usize> = HashSet::new();

        while exited.len() < processes.len() {
            for (idx, managed) in processes.iter_mut().enumerate() {
                // Collect output
                while let Some(line) = managed.output.try_recv() {
                    let formatted = formatter.format(&line);
                    println!("{}", formatted);
                }

                // Check if already exited
                if exited.contains(&idx) {
                    continue;
                }

                // Check readiness probe if not yet ready
                if !managed.is_ready {
                    if let Some(ref probe) = managed.ready_probe {
                        if readiness::is_ready(probe) {
                            managed.is_ready = true;
                            let msg =
                                formatter.format_control(&managed.name, "Ready (probe passed)");
                            println!("{}", msg);
                        }
                    }
                }

                // Check for exit
                if let Ok(Some(exit_status)) = managed.process.child.try_wait() {
                    let status = exit_status_to_process_status(&exit_status);

                    // Drain remaining output
                    while let Some(line) = managed.output.try_recv() {
                        let formatted = formatter.format(&line);
                        println!("{}", formatted);
                    }

                    // One-shot processes become ready on successful exit
                    if managed.is_oneshot && !managed.is_ready {
                        if status == ProcessStatus::Success {
                            managed.is_ready = true;
                            let msg = formatter
                                .format_control(&managed.name, "Ready (exited successfully)");
                            println!("{}", msg);
                        }
                    }

                    let msg = match status {
                        ProcessStatus::Success => {
                            formatter.format_control(&managed.name, &status.to_string())
                        }
                        _ => formatter.format_error(&managed.name, &status.to_string()),
                    };
                    println!("{}", msg);
                    exited.insert(idx);
                }
            }

            thread::sleep(poll_interval);
        }

        Ok(())
    }
}

fn exit_status_to_process_status(exit_status: &std::process::ExitStatus) -> ProcessStatus {
    if exit_status.success() {
        ProcessStatus::Success
    } else if let Some(code) = exit_status.code() {
        ProcessStatus::Failed(code)
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = exit_status.signal() {
                return ProcessStatus::Signaled(sig);
            }
        }
        ProcessStatus::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProcessDef, ProcessOptions};

    fn simple_procfile(processes: Vec<(&str, &str)>) -> Procfile {
        Procfile {
            processes: processes
                .into_iter()
                .map(|(name, cmd)| ProcessDef {
                    name: name.to_string(),
                    watch_patterns: vec![],
                    options: ProcessOptions::default(),
                    command: cmd.to_string(),
                    oneshot: true,
                })
                .collect(),
        }
    }

    #[test]
    fn test_run_single_process() {
        let procfile = simple_procfile(vec![("test", "echo hello")]);
        let orchestrator = Orchestrator::new(procfile, std::env::current_dir().unwrap());
        orchestrator.run().unwrap();
    }

    #[test]
    fn test_run_multiple_processes() {
        let procfile = simple_procfile(vec![
            ("one", "echo one"),
            ("two", "echo two"),
            ("three", "echo three"),
        ]);
        let orchestrator = Orchestrator::new(procfile, std::env::current_dir().unwrap());
        orchestrator.run().unwrap();
    }

    #[test]
    fn test_run_empty_procfile() {
        let procfile = Procfile { processes: vec![] };
        let orchestrator = Orchestrator::new(procfile, std::env::current_dir().unwrap());
        orchestrator.run().unwrap();
    }

    #[test]
    fn test_process_failure_logged() {
        let procfile = simple_procfile(vec![("fail", "exit 42")]);
        let orchestrator = Orchestrator::new(procfile, std::env::current_dir().unwrap());
        // Should complete without error (failures are logged, not propagated)
        orchestrator.run().unwrap();
    }
}
