mod graph;
pub mod runner;
mod watcher;

use crate::output::{ControlEvent, OutputFormatter};
use crate::parser::{ProcessDef, Procfile, Signal};
use crate::readiness;
use graph::DependencyGraph;
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
    ready_probe_started: Option<Instant>,
    last_probe_check: Option<Instant>,
    started: bool, // Has this process been started at least once?
    reloading: bool,
    reload_signal_sent: Option<Instant>,
    reload_path: Option<String>,
    // Crash recovery state
    consecutive_failures: u32,
    last_start_time: Option<Instant>,
    scheduled_restart: Option<Instant>,
    last_backoff_decrease: Option<Instant>,
}

impl ManagedProcess {
    fn new(def: ProcessDef) -> Self {
        Self {
            def,
            process: None,
            output: None,
            is_ready: false,
            ready_probe_started: None,
            last_probe_check: None,
            started: false,
            reloading: false,
            reload_signal_sent: None,
            reload_path: None,
            consecutive_failures: 0,
            last_start_time: None,
            scheduled_restart: None,
            last_backoff_decrease: None,
        }
    }

    fn is_running(&self) -> bool {
        self.process.is_some()
    }

    fn backoff_for_failures(failures: u32) -> Duration {
        if failures == 0 {
            Duration::ZERO
        } else {
            // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s (capped)
            let secs = 1u64 << (failures - 1).min(5);
            Duration::from_secs(secs)
        }
    }

    fn calculate_backoff(&self) -> Duration {
        Self::backoff_for_failures(self.consecutive_failures)
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
            ProcessStatus::Success => write!(f, "exit 0"),
            ProcessStatus::Failed(code) => write!(f, "exit {}", code),
            ProcessStatus::Signaled(sig) => {
                let name = match sig {
                    1 => "SIGHUP",
                    2 => "SIGINT",
                    9 => "SIGKILL",
                    15 => "SIGTERM",
                    17 => "SIGCHLD",
                    19 => "SIGSTOP",
                    _ => return write!(f, "signal {}", sig),
                };
                write!(f, "{}", name)
            }
            ProcessStatus::Unknown => write!(f, "exit ?"),
        }
    }
}

impl Orchestrator {
    pub fn new(procfile: Procfile, base_dir: std::path::PathBuf) -> Self {
        Self { procfile, base_dir }
    }

    /// Start any processes whose dependencies are now all satisfied.
    fn start_ready_dependents(
        &self,
        processes: &mut HashMap<String, ManagedProcess>,
        graph: &DependencyGraph,
        formatter: &OutputFormatter,
        guard: &mut ShutdownGuard,
    ) {
        // Find processes that haven't started but whose dependencies are all ready
        // Collect (name, deps) so we can show what we were waiting for
        let to_start: Vec<(String, Vec<String>)> = processes
            .iter()
            .filter(|(_, m)| !m.started)
            .filter(|(name, _)| {
                graph
                    .dependencies_of(name)
                    .iter()
                    .all(|dep| processes.get(*dep).map(|m| m.is_ready).unwrap_or(false))
            })
            .map(|(name, _)| {
                let deps: Vec<String> = graph
                    .dependencies_of(name)
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                (name.clone(), deps)
            })
            .collect();

        for (name, deps) in to_start {
            let after = if deps.is_empty() {
                None
            } else {
                Some(deps.join(", "))
            };
            if let Err(e) = self.spawn_managed(processes, &name, formatter, guard, after.as_deref())
            {
                let msg = formatter.format_control(
                    &name,
                    ControlEvent::Crashed,
                    &format!("failed to start: {}", e),
                );
                println!("{}", msg);
            }
        }
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

        // Build dependency graph
        let graph = DependencyGraph::new(
            self.procfile
                .processes
                .iter()
                .map(|p| (p.name.as_str(), p.options.after.as_slice())),
        );

        // Guard to ensure cleanup on panic
        let mut guard = ShutdownGuard::new();

        // Spawn only root processes (those with no dependencies)
        for name in graph.roots() {
            self.spawn_managed(&mut processes, name, &formatter, &mut guard, None)?;
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
                    let msg = formatter.format_control(
                        "proctor",
                        ControlEvent::Crashed,
                        &format!("watcher error: {}", e),
                    );
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
        // Track which processes have been signaled during shutdown (in order)
        let shutdown_order: Vec<String> = graph
            .reverse_topological_order()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let mut shutdown_signaled: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        loop {
            // Check for shutdown signal
            if !shutting_down && shutdown_flag.load(Ordering::SeqCst) {
                shutting_down = true;
            }

            // During shutdown: signal processes in reverse dependency order
            // Only signal a process once all its dependents have exited
            if shutting_down {
                for name in &shutdown_order {
                    if shutdown_signaled.contains(name) {
                        continue;
                    }
                    // Check if all dependents have exited
                    let dependents_exited = graph
                        .dependents_of(name)
                        .iter()
                        .all(|dep| !processes.get(*dep).map(|m| m.is_running()).unwrap_or(false));

                    if !dependents_exited {
                        continue;
                    }

                    if let Some(managed) = processes.get_mut(name) {
                        if let Some(ref proc) = managed.process {
                            let msg = formatter.format_control(
                                &managed.def.name,
                                ControlEvent::Stopped,
                                "kill -TERM",
                            );
                            println!("{}", msg);
                            let _ = proc.signal(Signal::Term);
                            managed.reload_signal_sent = Some(Instant::now());
                        }
                    }
                    shutdown_signaled.insert(name.clone());
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

            // Check readiness probes (every 250ms per process)
            let probe_timeout = Duration::from_secs(30);
            let probe_interval = Duration::from_millis(250);
            for managed in processes.values_mut() {
                if managed.is_ready || !managed.is_running() {
                    continue;
                }

                if let Some(ref probe) = managed.def.options.ready {
                    // Start tracking probe time if not already
                    if managed.ready_probe_started.is_none() {
                        managed.ready_probe_started = Some(Instant::now());
                    }

                    // Rate limit probe checks to 250ms intervals
                    let should_check = managed
                        .last_probe_check
                        .map(|t| t.elapsed() >= probe_interval)
                        .unwrap_or(true);

                    if !should_check {
                        continue;
                    }
                    managed.last_probe_check = Some(Instant::now());

                    if readiness::is_ready(probe) {
                        managed.is_ready = true;
                        managed.ready_probe_started = None;
                        let msg = formatter.format_control(
                            &managed.def.name,
                            ControlEvent::Ready,
                            "probe passed",
                        );
                        println!("{}", msg);
                    } else if let Some(started) = managed.ready_probe_started {
                        let elapsed = started.elapsed();
                        if elapsed >= probe_timeout {
                            managed.ready_probe_started = None;
                            let msg = formatter.format_control(
                                &managed.def.name,
                                ControlEvent::TimedOut,
                                "probe timed out after 30s (aborting)",
                            );
                            println!("{}", msg);
                            shutting_down = true;
                        } else {
                            // Log every 5s after initial 5s grace period
                            let secs = elapsed.as_secs();
                            if secs >= 5 && secs % 5 == 0 && elapsed.subsec_millis() < 300 {
                                let msg = formatter.format_control(
                                    &managed.def.name,
                                    ControlEvent::TimedOut,
                                    &format!("probe pending ({}s)", secs),
                                );
                                println!("{}", msg);
                            }
                        }
                    }
                }
            }

            // Start any processes whose dependencies are now satisfied
            self.start_ready_dependents(&mut processes, &graph, &formatter, &mut guard);

            // Gradually decrease backoff level while running stably
            // If running for the duration of the current backoff level, decrease by one level
            let now = Instant::now();
            for managed in processes.values_mut() {
                if managed.is_running() && managed.consecutive_failures > 0 {
                    let current_backoff = managed.calculate_backoff();
                    let reference_time = managed
                        .last_backoff_decrease
                        .or(managed.last_start_time)
                        .unwrap_or(now);
                    if now.duration_since(reference_time) >= current_backoff {
                        managed.consecutive_failures -= 1;
                        managed.last_backoff_decrease = Some(now);
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
                let logged_ready = if managed.def.oneshot
                    && !managed.is_ready
                    && status == ProcessStatus::Success
                {
                    managed.is_ready = true;
                    let msg =
                        formatter.format_control(&name, ControlEvent::Ready, "exited successfully");
                    println!("{}", msg);
                    true
                } else {
                    false
                };

                // One-shot process failure aborts startup
                if managed.def.oneshot && status != ProcessStatus::Success && !shutting_down {
                    let msg = formatter.format_control(
                        &name,
                        ControlEvent::Crashed,
                        &format!("{} (aborting) ", status),
                    );
                    println!("{}", msg);
                    shutting_down = true;
                }

                // Clean up
                managed.process = None;
                managed.output = None;

                // Handle reload completion (but not during shutdown)
                if managed.reloading && !shutting_down {
                    let path = managed.reload_path.take().unwrap_or_default();
                    managed.reloading = false;
                    managed.reload_signal_sent = None;
                    // Reset failure count on intentional reload
                    managed.consecutive_failures = 0;
                    let msg = formatter.format_control(&name, ControlEvent::Restarting, &path);
                    println!("{}", msg);
                    if let Err(e) =
                        self.spawn_managed(&mut processes, &name, &formatter, &mut guard, None)
                    {
                        let msg = formatter.format_control(
                            &name,
                            ControlEvent::Crashed,
                            &format!("failed to restart: {}", e),
                        );
                        println!("{}", msg);
                    }
                } else if !shutting_down && !managed.def.oneshot {
                    // Crash recovery for long-running processes (any exit is unexpected)
                    managed.consecutive_failures += 1;
                    let backoff = managed.calculate_backoff();
                    let restart_time = Instant::now() + backoff;
                    managed.scheduled_restart = Some(restart_time);

                    let detail = if status == ProcessStatus::Success {
                        format!("{} (unexpectedly)", status)
                    } else {
                        status.to_string()
                    };
                    let msg = formatter.format_control(&name, ControlEvent::Crashed, &detail);
                    println!("{}", msg);

                    let msg = if backoff.is_zero() {
                        formatter.format_control(&name, ControlEvent::Restarting, "now")
                    } else {
                        formatter.format_control(
                            &name,
                            ControlEvent::Restarting,
                            &format!("in {}s", backoff.as_secs()),
                        )
                    };
                    println!("{}", msg);
                } else if !logged_ready && !shutting_down {
                    // Log exit status (skip if we logged ready, crash, or shutting down)
                    let event = if status == ProcessStatus::Success {
                        ControlEvent::Finished
                    } else {
                        ControlEvent::Stopped
                    };
                    let msg = formatter.format_control(&name, event, &status.to_string());
                    println!("{}", msg);
                }
            }

            // Start any dependents whose dependencies became ready (e.g., one-shot exit)
            if !shutting_down {
                self.start_ready_dependents(&mut processes, &graph, &formatter, &mut guard);
            }

            // Handle scheduled restarts (crash recovery)
            if !shutting_down {
                let names_to_restart: Vec<String> = processes
                    .iter()
                    .filter_map(|(name, managed)| {
                        if let Some(restart_time) = managed.scheduled_restart {
                            if Instant::now() >= restart_time {
                                return Some(name.clone());
                            }
                        }
                        None
                    })
                    .collect();

                for name in names_to_restart {
                    if let Err(e) =
                        self.spawn_managed(&mut processes, &name, &formatter, &mut guard, None)
                    {
                        let msg = formatter.format_control(
                            &name,
                            ControlEvent::Crashed,
                            &format!("failed to restart: {}", e),
                        );
                        println!("{}", msg);
                    }
                }
            }

            // During shutdown: check if all processes have exited
            if shutting_down {
                let all_exited = processes.values().all(|m| !m.is_running());
                if all_exited {
                    break;
                }

                // Check per-process shutdown timeout - SIGKILL stragglers individually
                let now = Instant::now();
                for managed in processes.values_mut() {
                    if let Some(signal_time) = managed.reload_signal_sent {
                        if now.duration_since(signal_time) >= managed.def.options.shutdown {
                            if let Some(ref proc) = managed.process {
                                let msg = formatter.format_control(
                                    &managed.def.name,
                                    ControlEvent::Stopped,
                                    "kill -9",
                                );
                                println!("{}", msg);
                                let _ = proc.kill();
                                // Clear signal_sent to avoid repeated SIGKILL attempts
                                managed.reload_signal_sent = None;
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
                                ControlEvent::Stopped,
                                "kill -9",
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
                                        ControlEvent::Restarting,
                                        &format!("kill -{}", signal_name_short(signal)),
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
        after: Option<&str>,
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
        managed.started = true;
        managed.last_start_time = Some(Instant::now());
        managed.scheduled_restart = None;
        managed.last_backoff_decrease = None;

        if is_ready {
            let detail = match after {
                Some(dep) => format!("started (after {})", dep),
                None => "started".to_string(),
            };
            let msg = formatter.format_control(name, ControlEvent::Ready, &detail);
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

fn signal_name_short(sig: Signal) -> &'static str {
    match sig {
        Signal::Hup => "HUP",
        Signal::Int => "INT",
        Signal::Term => "TERM",
        Signal::Kill => "9",
        Signal::Usr1 => "USR1",
        Signal::Usr2 => "USR2",
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

    #[test]
    fn test_backoff_calculation() {
        let def = ProcessDef {
            name: "test".to_string(),
            watch_patterns: vec![],
            options: ProcessOptions::default(),
            command: "echo test".to_string(),
            oneshot: false,
        };

        let mut managed = ManagedProcess::new(def);

        // No failures = no backoff
        assert_eq!(managed.calculate_backoff(), Duration::ZERO);

        // First failure = 1s
        managed.consecutive_failures = 1;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(1));

        // Second failure = 2s
        managed.consecutive_failures = 2;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(2));

        // Third failure = 4s
        managed.consecutive_failures = 3;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(4));

        // Fourth failure = 8s
        managed.consecutive_failures = 4;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(8));

        // Fifth failure = 16s
        managed.consecutive_failures = 5;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(16));

        // Sixth+ failure = 32s (capped)
        managed.consecutive_failures = 6;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(32));

        managed.consecutive_failures = 10;
        assert_eq!(managed.calculate_backoff(), Duration::from_secs(32));
    }
}
