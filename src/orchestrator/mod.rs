pub mod runner;
mod watcher;

use crate::output::OutputFormatter;
use crate::parser::{ProcessDef, Procfile, Signal};
use crate::readiness;
use runner::{ProcessOutput, RunningProcess, spawn_process};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use watcher::{Debouncer, FileWatcher};

pub struct Orchestrator {
    procfile: Procfile,
    base_dir: std::path::PathBuf,
}

/// Guard that ensures all process groups are killed when dropped (e.g., on panic)
struct ShutdownGuard {
    pgids: Vec<nix::unistd::Pid>,
    active: bool,
}

impl ShutdownGuard {
    fn new() -> Self {
        Self {
            pgids: Vec::new(),
            active: true,
        }
    }

    fn track(&mut self, pgid: nix::unistd::Pid) {
        if !self.pgids.contains(&pgid) {
            self.pgids.push(pgid);
        }
    }

    fn untrack(&mut self, pgid: nix::unistd::Pid) {
        self.pgids.retain(|&p| p != pgid);
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Kill all tracked process groups immediately
        for &pgid in &self.pgids {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

struct ManagedProcess {
    def: ProcessDef,
    process: Option<RunningProcess>,
    output: Option<ProcessOutput>,
    is_ready: bool,
    reloading: bool,
    reload_signal_sent: Option<Instant>,
    reload_path: Option<String>,
}

impl ManagedProcess {
    fn new(def: ProcessDef) -> Self {
        Self {
            def,
            process: None,
            output: None,
            is_ready: false,
            reloading: false,
            reload_signal_sent: None,
            reload_path: None,
        }
    }

    fn is_running(&self) -> bool {
        self.process.is_some()
    }
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

        // Set up signal handling
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = Arc::clone(&shutdown_flag);

        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        thread::spawn(move || {
            for _ in signals.forever() {
                shutdown_flag_clone.store(true, Ordering::SeqCst);
                break;
            }
        });

        let process_names: Vec<&str> = self
            .procfile
            .processes
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let formatter = OutputFormatter::new(&process_names);

        let mut processes: HashMap<String, ManagedProcess> = HashMap::new();

        // Initialize managed processes
        for def in &self.procfile.processes {
            processes.insert(def.name.clone(), ManagedProcess::new(def.clone()));
        }

        // Guard to ensure cleanup on panic
        let mut guard = ShutdownGuard::new();

        // Spawn all processes
        for def in &self.procfile.processes {
            self.spawn_managed(&mut processes, &def.name, &formatter, &mut guard)?;
        }

        // Set up file watcher if any process has watch patterns
        let watch_processes: Vec<_> = self
            .procfile
            .processes
            .iter()
            .filter(|p| !p.watch_patterns.is_empty())
            .map(|p| (p.name.clone(), p.watch_patterns.clone(), p.options.debounce))
            .collect();

        let mut watcher = if !watch_processes.is_empty() {
            let mut debouncer = Debouncer::new();
            for (name, _, debounce) in &watch_processes {
                debouncer.set_debounce(name, *debounce);
            }
            match FileWatcher::new(&self.base_dir, watch_processes) {
                Ok(w) => Some((w, debouncer)),
                Err(e) => {
                    let msg = formatter.format_error("proctor", &format!("watcher error: {}", e));
                    println!("{}", msg);
                    None
                }
            }
        } else {
            None
        };

        let poll_interval = Duration::from_millis(10);
        let has_long_running = self.procfile.processes.iter().any(|p| !p.oneshot);

        // Shutdown state
        let mut shutting_down = false;
        let mut shutdown_started: Option<Instant> = None;

        loop {
            // Check for shutdown signal
            if !shutting_down && shutdown_flag.load(Ordering::SeqCst) {
                shutting_down = true;
                shutdown_started = Some(Instant::now());
                let msg = formatter.format_control("proctor", "Shutting down...");
                println!("{}", msg);

                // Send SIGTERM to all running processes
                for managed in processes.values_mut() {
                    if let Some(ref proc) = managed.process {
                        let msg = formatter.format_control(&managed.def.name, "Sending SIGTERM");
                        println!("{}", msg);
                        let _ = proc.signal(Signal::Term);
                        managed.reload_signal_sent = Some(Instant::now());
                    }
                }
            }
            // Collect output from all processes
            for managed in processes.values_mut() {
                if let Some(ref output) = managed.output {
                    while let Some(line) = output.try_recv() {
                        let formatted = formatter.format(&line);
                        println!("{}", formatted);
                    }
                }
            }

            // Check readiness probes
            for managed in processes.values_mut() {
                if managed.is_ready || !managed.is_running() {
                    continue;
                }

                if let Some(ref probe) = managed.def.options.ready {
                    if readiness::is_ready(probe) {
                        managed.is_ready = true;
                        let msg =
                            formatter.format_control(&managed.def.name, "Ready (probe passed)");
                        println!("{}", msg);
                    }
                }
            }

            // Check for process exits
            let mut exited = Vec::new();
            for (name, managed) in processes.iter_mut() {
                if let Some(ref mut proc) = managed.process {
                    if let Ok(Some(exit_status)) = proc.child.try_wait() {
                        exited.push((name.clone(), exit_status, proc.pgid));
                    }
                }
            }

            for (name, exit_status, pgid) in exited {
                let status = exit_status_to_process_status(&exit_status);
                let managed = processes.get_mut(&name).unwrap();

                // Untrack from guard
                guard.untrack(pgid);

                // Drain remaining output
                if let Some(ref output) = managed.output {
                    while let Some(line) = output.try_recv() {
                        let formatted = formatter.format(&line);
                        println!("{}", formatted);
                    }
                }

                // One-shot processes become ready on successful exit
                if managed.def.oneshot && !managed.is_ready && status == ProcessStatus::Success {
                    managed.is_ready = true;
                    let msg = formatter.format_control(&name, "Ready (exited successfully)");
                    println!("{}", msg);
                }

                // Log exit status
                let msg = match status {
                    ProcessStatus::Success => formatter.format_control(&name, &status.to_string()),
                    _ => formatter.format_error(&name, &status.to_string()),
                };
                println!("{}", msg);

                // Clean up
                managed.process = None;
                managed.output = None;

                // Handle reload completion (but not during shutdown)
                if managed.reloading && !shutting_down {
                    let path = managed.reload_path.take().unwrap_or_default();
                    managed.reloading = false;
                    managed.reload_signal_sent = None;
                    let msg = formatter.format_control(&name, &format!("Restarting ({})", path));
                    println!("{}", msg);
                    if let Err(e) =
                        self.spawn_managed(&mut processes, &name, &formatter, &mut guard)
                    {
                        let msg =
                            formatter.format_error(&name, &format!("failed to restart: {}", e));
                        println!("{}", msg);
                    }
                }
            }

            // During shutdown: check if all processes have exited
            if shutting_down {
                let all_exited = processes.values().all(|m| !m.is_running());
                if all_exited {
                    let msg = formatter.format_control("proctor", "All processes stopped");
                    println!("{}", msg);
                    break;
                }

                // Check shutdown timeout - SIGKILL stragglers
                if let Some(started) = shutdown_started {
                    let max_shutdown = Duration::from_secs(5);
                    if Instant::now().duration_since(started) >= max_shutdown {
                        for managed in processes.values_mut() {
                            if let Some(ref proc) = managed.process {
                                let msg = formatter.format_control(
                                    &managed.def.name,
                                    "Sending SIGKILL (shutdown timeout)",
                                );
                                println!("{}", msg);
                                let _ = proc.kill();
                            }
                        }
                    }
                }
            }

            // Check for reload timeouts (SIGKILL if needed)
            let now = Instant::now();
            for managed in processes.values_mut() {
                if let Some(signal_time) = managed.reload_signal_sent {
                    if now.duration_since(signal_time) >= managed.def.options.shutdown {
                        if let Some(ref proc) = managed.process {
                            let msg = formatter.format_control(
                                &managed.def.name,
                                "Sending SIGKILL (shutdown timeout)",
                            );
                            println!("{}", msg);
                            let _ = proc.kill();
                        }
                    }
                }
            }

            // Handle file watcher events (skip during shutdown)
            if !shutting_down {
                if let Some((ref watcher, ref mut debouncer)) = watcher {
                    while let Some(event) = watcher.try_recv() {
                        debouncer.record_event(&event.process, &event.path);
                    }

                    for (name, path) in debouncer.get_ready() {
                        if let Some(managed) = processes.get_mut(&name) {
                            if managed.is_running() && !managed.reloading {
                                managed.reloading = true;
                                managed.reload_signal_sent = Some(Instant::now());
                                managed.reload_path = Some(path);
                                let signal = managed.def.options.signal;
                                if let Some(ref proc) = managed.process {
                                    let msg = formatter.format_control(
                                        &name,
                                        &format!("Reloading (sending {})", signal_name(signal)),
                                    );
                                    println!("{}", msg);
                                    let _ = proc.signal(signal);
                                }
                            }
                        }
                    }
                }
            }

            // Check if we should exit
            if !has_long_running {
                // All processes are one-shot - exit when all are done
                let all_done = processes.values().all(|m| !m.is_running());
                if all_done {
                    break;
                }
            } else if watcher.is_none() {
                // No file watcher - exit when all processes exit
                let all_done = processes.values().all(|m| !m.is_running());
                if all_done {
                    break;
                }
            }
            // With a file watcher and long-running processes, keep running

            thread::sleep(poll_interval);
        }

        // Disarm the guard - we've cleaned up properly
        guard.disarm();

        Ok(())
    }

    fn spawn_managed(
        &self,
        processes: &mut HashMap<String, ManagedProcess>,
        name: &str,
        formatter: &OutputFormatter,
        guard: &mut ShutdownGuard,
    ) -> io::Result<()> {
        let managed = processes.get_mut(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("process '{}' not found", name),
            )
        })?;

        let mut running = spawn_process(&managed.def, &self.base_dir, None)?;
        let output = running.take_output().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "failed to capture process output")
        })?;

        // Track the process group for cleanup
        guard.track(running.pgid);

        let is_oneshot = managed.def.oneshot;
        let has_probe = managed.def.options.ready.is_some();

        // Long-running processes without probes are ready immediately
        let is_ready = !is_oneshot && !has_probe;

        managed.process = Some(running);
        managed.output = Some(output);
        managed.is_ready = is_ready;

        if is_ready {
            let msg = formatter.format_control(name, "Ready (started)");
            println!("{}", msg);
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

fn signal_name(sig: Signal) -> &'static str {
    match sig {
        Signal::Hup => "SIGHUP",
        Signal::Int => "SIGINT",
        Signal::Term => "SIGTERM",
        Signal::Kill => "SIGKILL",
        Signal::Usr1 => "SIGUSR1",
        Signal::Usr2 => "SIGUSR2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ProcessOptions;

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
        orchestrator.run().unwrap();
    }
}
