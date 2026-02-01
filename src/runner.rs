#![allow(dead_code)]

use crate::parser::{ProcessDef, Signal};
use nix::sys::signal::{Signal as NixSignal, killpg};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSource {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct OutputLine {
    pub process: String,
    pub source: OutputSource,
    pub content: String,
}

pub struct ProcessOutput {
    pub receiver: Receiver<OutputLine>,
    stdout_handle: Option<JoinHandle<()>>,
    stderr_handle: Option<JoinHandle<()>>,
}

impl ProcessOutput {
    pub fn new(name: String, stdout: ChildStdout, stderr: ChildStderr) -> Self {
        let (tx, rx) = mpsc::channel();

        let stdout_handle = spawn_reader(name.clone(), OutputSource::Stdout, stdout, tx.clone());
        let stderr_handle = spawn_reader(name, OutputSource::Stderr, stderr, tx);

        Self {
            receiver: rx,
            stdout_handle: Some(stdout_handle),
            stderr_handle: Some(stderr_handle),
        }
    }

    pub fn try_recv(&self) -> Option<OutputLine> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for ProcessOutput {
    fn drop(&mut self) {
        if let Some(h) = self.stdout_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn_reader<R: io::Read + Send + 'static>(
    process: String,
    source: OutputSource,
    reader: R,
    tx: Sender<OutputLine>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            match line {
                Ok(content) => {
                    let msg = OutputLine {
                        process: process.clone(),
                        source: source.clone(),
                        content,
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
}

#[derive(Debug)]
pub struct RunningProcess {
    pub name: String,
    pub child: Child,
    pub pgid: Pid,
}

impl RunningProcess {
    pub fn signal(&self, sig: Signal) -> io::Result<()> {
        killpg(self.pgid, sig.to_nix()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn kill(&self) -> io::Result<()> {
        killpg(self.pgid, NixSignal::SIGKILL).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn take_output(&mut self) -> Option<ProcessOutput> {
        let stdout = self.child.stdout.take()?;
        let stderr = self.child.stderr.take()?;
        Some(ProcessOutput::new(self.name.clone(), stdout, stderr))
    }
}

pub fn spawn_process(
    def: &ProcessDef,
    base_dir: &Path,
    extra_env: Option<&HashMap<String, String>>,
) -> io::Result<RunningProcess> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let working_dir = if let Some(ref dir) = def.options.dir {
        base_dir.join(dir)
    } else {
        base_dir.to_path_buf()
    };

    let mut cmd = Command::new(&shell);
    cmd.arg("-c").arg(&def.command);
    cmd.current_dir(&working_dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    if let Some(env) = extra_env {
        cmd.envs(env);
    }

    // Create a new process group so we can signal the entire group
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        });
    }

    let child = cmd.spawn()?;
    let pid = Pid::from_raw(child.id() as i32);

    Ok(RunningProcess {
        name: def.name.clone(),
        child,
        pgid: pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ProcessOptions;
    use std::time::Duration;

    fn simple_def(name: &str, command: &str) -> ProcessDef {
        ProcessDef {
            name: name.to_string(),
            watch_patterns: vec![],
            options: ProcessOptions::default(),
            command: command.to_string(),
        }
    }

    #[test]
    fn test_spawn_simple_command() {
        let def = simple_def("test", "echo hello");
        let mut proc = spawn_process(&def, Path::new("."), None).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let line = output
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(line.content, "hello");
        assert_eq!(line.source, OutputSource::Stdout);
        assert_eq!(line.process, "test");
    }

    #[test]
    fn test_spawn_with_env() {
        let def = simple_def("test", "echo $MY_VAR");
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "test_value".to_string());

        let mut proc = spawn_process(&def, Path::new("."), Some(&env)).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let line = output
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(line.content, "test_value");
    }

    #[test]
    fn test_spawn_with_working_dir() {
        let def = ProcessDef {
            name: "test".to_string(),
            watch_patterns: vec![],
            options: ProcessOptions {
                dir: Some("src".to_string()),
                ..Default::default()
            },
            command: "pwd".to_string(),
        };

        let base = std::env::current_dir().unwrap();
        let mut proc = spawn_process(&def, &base, None).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let line = output
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(line.content.ends_with("/src"));
    }

    #[test]
    fn test_signal_process() {
        let def = simple_def("test", "sleep 60");
        let proc = spawn_process(&def, Path::new("."), None).unwrap();

        proc.signal(Signal::Term).unwrap();

        let mut child = proc.child;
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn test_capture_stderr() {
        let def = simple_def("test", "echo error >&2");
        let mut proc = spawn_process(&def, Path::new("."), None).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let line = output
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(line.content, "error");
        assert_eq!(line.source, OutputSource::Stderr);
    }

    #[test]
    fn test_capture_multiple_lines() {
        let def = simple_def("test", "echo one; echo two; echo three");
        let mut proc = spawn_process(&def, Path::new("."), None).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let mut lines = Vec::new();
        while let Ok(line) = output.receiver.recv_timeout(Duration::from_millis(100)) {
            lines.push(line.content);
        }
        assert_eq!(lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn test_capture_mixed_stdout_stderr() {
        let def = simple_def("test", "echo out1; echo err1 >&2; echo out2");
        let mut proc = spawn_process(&def, Path::new("."), None).unwrap();

        let output = proc.take_output().unwrap();
        proc.child.wait().unwrap();

        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();
        while let Ok(line) = output.receiver.recv_timeout(Duration::from_millis(100)) {
            match line.source {
                OutputSource::Stdout => stdout_lines.push(line.content),
                OutputSource::Stderr => stderr_lines.push(line.content),
            }
        }
        assert_eq!(stdout_lines, vec!["out1", "out2"]);
        assert_eq!(stderr_lines, vec!["err1"]);
    }
}
