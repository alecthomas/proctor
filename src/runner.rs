#![allow(dead_code)]

use crate::parser::{ProcessDef, Signal};
use nix::sys::signal::{Signal as NixSignal, killpg};
use nix::unistd::Pid;
use std::collections::HashMap;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

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
    use std::io::{BufRead, BufReader};

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

        let stdout = proc.child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "hello");
    }

    #[test]
    fn test_spawn_with_env() {
        let def = simple_def("test", "echo $MY_VAR");
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "test_value".to_string());

        let mut proc = spawn_process(&def, Path::new("."), Some(&env)).unwrap();

        let stdout = proc.child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "test_value");
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

        let stdout = proc.child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.trim().ends_with("/src"));
    }

    #[test]
    fn test_signal_process() {
        let def = simple_def("test", "sleep 60");
        let proc = spawn_process(&def, Path::new("."), None).unwrap();

        // Signal should succeed
        proc.signal(Signal::Term).unwrap();

        // Process should terminate
        let mut child = proc.child;
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
