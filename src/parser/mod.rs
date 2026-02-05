#![allow(dead_code)]

mod ast;
mod tokenizer;

pub use ast::*;

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, " {}", self.message)
        } else {
            write!(f, "{}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Procfile, ParseError> {
    use tokenizer::{ProcfileItem, parse_procfile};

    let mut input_str = input;
    let items = parse_procfile(&mut input_str).map_err(|_| ParseError {
        line: 0,
        message: "failed to parse procfile".to_string(),
    })?;

    let mut global_env = HashMap::new();
    let mut processes = Vec::new();

    for item in items {
        match item {
            ProcfileItem::GlobalEnv { key, value } => {
                global_env.insert(key, value);
            }
            ProcfileItem::Process(proc_line) => {
                let process = parse_declaration(&proc_line.declaration_tokens, &proc_line.command)?;
                processes.push(process);
            }
        }
    }

    // Check for duplicate process names
    let mut seen = std::collections::HashSet::new();
    for proc in &processes {
        if !seen.insert(&proc.name) {
            return Err(ParseError {
                line: 0,
                message: format!("duplicate process name: {}", proc.name),
            });
        }
    }

    // Check for circular dependencies
    check_circular_deps(&processes)?;

    Ok(Procfile { global_env, processes })
}

fn parse_declaration(decl_tokens: &[String], command: &str) -> Result<ProcessDef, ParseError> {
    if decl_tokens.is_empty() {
        return Err(ParseError {
            line: 0,
            message: "missing process name".to_string(),
        });
    }

    // Parse declaration tokens - check for oneshot suffix (name!)
    let raw_name = &decl_tokens[0];
    let (name, oneshot) = if let Some(n) = raw_name.strip_suffix('!') {
        (n.to_string(), true)
    } else {
        (raw_name.clone(), false)
    };

    if !is_valid_name(&name) {
        return Err(ParseError {
            line: 0,
            message: format!("invalid process name: {}", name),
        });
    }

    let mut watch_patterns = Vec::new();
    let mut options = ProcessOptions::default();

    let mut i = 1;
    while i < decl_tokens.len() {
        let token = &decl_tokens[i];

        // Check for key=value option (three tokens: key, =, value)
        if i + 2 < decl_tokens.len() && decl_tokens[i + 1] == "=" {
            let key = token;
            let mut value = decl_tokens[i + 2].clone();
            i += 3;
            // For ready= option, consume additional =value suffix if present
            // (handles http:port/path=status format)
            if key == "ready" && i + 1 < decl_tokens.len() && decl_tokens[i] == "=" {
                value.push('=');
                value.push_str(&decl_tokens[i + 1]);
                i += 2;
            }
            apply_option(&mut options, key, &value, 0)?;
        } else {
            // Any non-option token is treated as a watch pattern (glob or bare file path)
            let (pattern, exclude) = if let Some(p) = token.strip_prefix('!') {
                (p.to_string(), true)
            } else {
                (token.clone(), false)
            };
            watch_patterns.push(GlobPattern { pattern, exclude });
            i += 1;
        }
    }

    if command.is_empty() {
        return Err(ParseError {
            line: 0,
            message: "missing command".to_string(),
        });
    }

    // Validate option combinations
    if oneshot && options.ready.is_some() {
        return Err(ParseError {
            line: 0,
            message: "one-shot processes cannot have a ready probe (they become ready on exit)".to_string(),
        });
    }

    Ok(ProcessDef {
        name,
        watch_patterns,
        options,
        command: command.to_string(),
        oneshot,
    })
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn apply_option(opts: &mut ProcessOptions, key: &str, value: &str, line_num: usize) -> Result<(), ParseError> {
    match key {
        "after" => {
            opts.after = value.split(',').map(|s| s.trim().to_string()).collect();
        }
        "ready" => {
            opts.ready = Some(parse_ready_probe(value, line_num)?);
        }
        "signal" => {
            opts.signal = Signal::from_str(value).ok_or_else(|| ParseError {
                line: line_num,
                message: format!("invalid signal: {}", value),
            })?;
        }
        "debounce" => {
            opts.debounce = parse_duration(value, line_num)?;
        }
        "dir" => {
            opts.dir = Some(value.to_string());
        }
        "shutdown" => {
            opts.shutdown = parse_duration(value, line_num)?;
        }
        _ => {
            return Err(ParseError {
                line: line_num,
                message: format!("unknown option: {}", key),
            });
        }
    }
    Ok(())
}

fn parse_ready_probe(value: &str, line_num: usize) -> Result<ReadyProbe, ParseError> {
    if let Some(rest) = value.strip_prefix("http:") {
        // Parse format: <port>[/<path>][=<status>]
        // First, check for =<status> suffix
        let (rest, expected_status) = if let Some(idx) = rest.rfind('=') {
            let status_str = &rest[idx + 1..];
            let status: u16 = status_str.parse().map_err(|_| ParseError {
                line: line_num,
                message: format!("invalid status code in ready probe: {}", status_str),
            })?;
            (&rest[..idx], Some(status))
        } else {
            (rest, None)
        };

        let (port_str, path) = if let Some(idx) = rest.find('/') {
            (&rest[..idx], rest[idx..].to_string())
        } else {
            (rest, "/".to_string())
        };
        let port: u16 = port_str.parse().map_err(|_| ParseError {
            line: line_num,
            message: format!("invalid port in ready probe: {}", port_str),
        })?;
        Ok(ReadyProbe::Http {
            port,
            path,
            expected_status,
        })
    } else if let Some(rest) = value.strip_prefix("exec:") {
        // Parse format: exec:<command> where command may be quoted
        let command = rest.trim();
        if command.is_empty() {
            return Err(ParseError {
                line: line_num,
                message: "exec probe requires a command".to_string(),
            });
        }
        Ok(ReadyProbe::Exec {
            command: command.to_string(),
        })
    } else {
        // Default: bare port number means TCP probe
        let port: u16 = value.parse().map_err(|_| ParseError {
            line: line_num,
            message: format!("invalid ready probe format: {}", value),
        })?;
        Ok(ReadyProbe::Tcp { port })
    }
}

fn parse_duration(value: &str, line_num: usize) -> Result<Duration, ParseError> {
    let err = || ParseError {
        line: line_num,
        message: format!("invalid duration: {}", value),
    };

    if let Some(ms) = value.strip_suffix("ms") {
        let n: u64 = ms.parse().map_err(|_| err())?;
        Ok(Duration::from_millis(n))
    } else if let Some(s) = value.strip_suffix('s') {
        let n: u64 = s.parse().map_err(|_| err())?;
        Ok(Duration::from_secs(n))
    } else if let Some(m) = value.strip_suffix('m') {
        let n: u64 = m.parse().map_err(|_| err())?;
        Ok(Duration::from_secs(n * 60))
    } else {
        Err(err())
    }
}

fn check_circular_deps(processes: &[ProcessDef]) -> Result<(), ParseError> {
    use std::collections::HashMap;

    let name_to_idx: HashMap<&str, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();

    // Check all dependencies exist
    for proc in processes {
        for dep in &proc.options.after {
            if !name_to_idx.contains_key(dep.as_str()) {
                return Err(ParseError {
                    line: 0,
                    message: format!("process '{}' depends on unknown process '{}'", proc.name, dep),
                });
            }
        }
    }

    // DFS for cycle detection
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        Visiting,
        Visited,
    }

    let mut state = vec![State::Unvisited; processes.len()];

    fn visit(
        idx: usize,
        processes: &[ProcessDef],
        name_to_idx: &HashMap<&str, usize>,
        state: &mut [State],
        path: &mut Vec<String>,
    ) -> Result<(), ParseError> {
        match state[idx] {
            State::Visited => return Ok(()),
            State::Visiting => {
                path.push(processes[idx].name.clone());
                return Err(ParseError {
                    line: 0,
                    message: format!("circular dependency: {}", path.join(" -> ")),
                });
            }
            State::Unvisited => {}
        }

        state[idx] = State::Visiting;
        path.push(processes[idx].name.clone());

        for dep in &processes[idx].options.after {
            let dep_idx = name_to_idx[dep.as_str()];
            visit(dep_idx, processes, name_to_idx, state, path)?;
        }

        path.pop();
        state[idx] = State::Visited;
        Ok(())
    }

    for i in 0..processes.len() {
        let mut path = Vec::new();
        visit(i, processes, &name_to_idx, &mut state, &mut path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_process() {
        let input = "api: go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 1);
        assert_eq!(procfile.processes[0].name, "api");
        assert_eq!(procfile.processes[0].command, "go run ./cmd/api");
        assert!(!procfile.processes[0].oneshot);
    }

    #[test]
    fn test_oneshot_process() {
        let input = "migrate!: just db migrate";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 1);
        assert_eq!(procfile.processes[0].name, "migrate");
        assert!(procfile.processes[0].oneshot);
    }

    #[test]
    fn test_with_glob() {
        let input = "api **/*.go: go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].watch_patterns.len(), 1);
        assert_eq!(procfile.processes[0].watch_patterns[0].pattern, "**/*.go");
        assert!(!procfile.processes[0].watch_patterns[0].exclude);
    }

    #[test]
    fn test_with_bare_file_path() {
        let input = "echo Procfile: echo hello";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].name, "echo");
        assert_eq!(procfile.processes[0].watch_patterns.len(), 1);
        assert_eq!(procfile.processes[0].watch_patterns[0].pattern, "Procfile");
        assert!(!procfile.processes[0].watch_patterns[0].exclude);
    }

    #[test]
    fn test_with_exclude_glob() {
        let input = "api **/*.go !**_test.go: go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].watch_patterns.len(), 2);
        assert!(!procfile.processes[0].watch_patterns[0].exclude);
        assert!(procfile.processes[0].watch_patterns[1].exclude);
    }

    #[test]
    fn test_with_options() {
        let input = "db: postgres\napi after=db debounce=1s: go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[1].options.after, vec!["db"]);
        assert_eq!(procfile.processes[1].options.debounce, Duration::from_secs(1));
    }

    #[test]
    fn test_with_env() {
        let input = "api: CGO_ENABLED=0 go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].command, "CGO_ENABLED=0 go run ./cmd/api");
    }

    #[test]
    fn test_ready_probe_tcp() {
        let input = "db ready=5432: postgres";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Tcp { port: 5432 })
        );
    }

    #[test]
    fn test_ready_probe_http() {
        let input = "api ready=http:8080/health: ./api";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Http {
                port: 8080,
                path: "/health".to_string(),
                expected_status: None,
            })
        );
    }

    #[test]
    fn test_ready_probe_http_with_status() {
        let input = "api ready=http:8080/health=200: ./api";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Http {
                port: 8080,
                path: "/health".to_string(),
                expected_status: Some(200),
            })
        );
    }

    #[test]
    fn test_ready_probe_http_with_status_no_path() {
        let input = "api ready=http:8080=201: ./api";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Http {
                port: 8080,
                path: "/".to_string(),
                expected_status: Some(201),
            })
        );
    }

    #[test]
    fn test_ready_probe_exec() {
        let input = r#"api ready=exec:"pg_isready -h localhost": ./api"#;
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Exec {
                command: "pg_isready -h localhost".to_string(),
            })
        );
    }

    #[test]
    fn test_ready_probe_exec_single_quoted() {
        let input = "api ready=exec:'test -f /tmp/ready': ./api";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Exec {
                command: "test -f /tmp/ready".to_string(),
            })
        );
    }

    #[test]
    fn test_ready_probe_exec_empty_error() {
        let input = "api ready=exec:: ./api";
        let err = parse(input).unwrap_err();
        assert!(err.message.contains("exec probe requires a command"));
    }

    #[test]
    fn test_comments_and_blanks() {
        let input = r#"
# This is a comment
api: go run ./cmd/api

# Another comment
worker: go run ./cmd/worker
"#;
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 2);
    }

    #[test]
    fn test_line_continuation() {
        let input = "api: go run \\\n  -tags dev \\\n  ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].command, "go run -tags dev ./cmd/api");
    }

    #[test]
    fn test_multiline_command_block() {
        let input = "api:\n    echo hello\n    echo world";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].command, "echo hello\necho world");
    }

    #[test]
    fn test_multiline_command_block_with_options() {
        let input = "api ready=8080:\n    go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes[0].command, "go run ./cmd/api");
        assert_eq!(
            procfile.processes[0].options.ready,
            Some(ReadyProbe::Tcp { port: 8080 })
        );
    }

    #[test]
    fn test_multiline_followed_by_another_process() {
        let input = "api:\n    echo hello\nworker: echo world";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 2);
        assert_eq!(procfile.processes[0].command, "echo hello");
        assert_eq!(procfile.processes[1].command, "echo world");
    }

    #[test]
    fn test_duplicate_name_error() {
        let input = "api: cmd1\napi: cmd2";
        let err = parse(input).unwrap_err();
        assert!(err.message.contains("duplicate"));
    }

    #[test]
    fn test_circular_dep_error() {
        let input = "a after=b: cmd\nb after=a: cmd";
        let err = parse(input).unwrap_err();
        assert!(err.message.contains("circular"));
    }

    #[test]
    fn test_unknown_dep_error() {
        let input = "api after=unknown: cmd";
        let err = parse(input).unwrap_err();
        assert!(err.message.contains("unknown process"));
    }

    #[test]
    fn test_oneshot_with_ready_error() {
        let input = "migrate! ready=5432: just db migrate";
        let err = parse(input).unwrap_err();
        assert!(err.message.contains("one-shot") && err.message.contains("ready"));
    }

    #[test]
    fn test_oneshot_with_watch_patterns() {
        let input = "migrate! **/*.sql !**/test_*.sql debounce=1s signal=INT: just db migrate";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 1);
        assert!(procfile.processes[0].oneshot);
        assert_eq!(procfile.processes[0].watch_patterns.len(), 2);
        assert_eq!(procfile.processes[0].watch_patterns[0].pattern, "**/*.sql");
        assert!(!procfile.processes[0].watch_patterns[0].exclude);
        assert_eq!(procfile.processes[0].watch_patterns[1].pattern, "**/test_*.sql");
        assert!(procfile.processes[0].watch_patterns[1].exclude);
        assert_eq!(procfile.processes[0].options.debounce, Duration::from_secs(1));
        assert_eq!(procfile.processes[0].options.signal, Signal::Int);
    }

    #[test]
    fn test_oneshot_with_valid_options() {
        // after, dir, and shutdown are valid for one-shot processes
        let input = "init!: just db init\nmigrate! after=init dir=./db shutdown=10s: just db migrate";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 2);
        assert!(procfile.processes[1].oneshot);
        assert_eq!(procfile.processes[1].options.after, vec!["init"]);
        assert_eq!(procfile.processes[1].options.dir, Some("./db".to_string()));
        assert_eq!(procfile.processes[1].options.shutdown, Duration::from_secs(10));
    }

    #[test]
    fn test_full_example() {
        let input = r#"
# Setup
init: just db init
migrate after=init: just db migrate

# Infrastructure
redis: redis-server
postgres ready=5432: docker run --rm -p 5432:5432 postgres:16

# Services
api **/*.go !**_test.go after=postgres debounce=500ms: CGO_ENABLED=0 go run ./cmd/api
"#;
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.processes.len(), 5);

        let api = procfile.processes.iter().find(|p| p.name == "api").unwrap();
        assert_eq!(api.watch_patterns.len(), 2);
        assert_eq!(api.options.after, vec!["postgres"]);
        assert_eq!(api.options.debounce, Duration::from_millis(500));
        assert_eq!(api.command, "CGO_ENABLED=0 go run ./cmd/api");
    }

    #[test]
    fn test_global_env_bare() {
        let input = "MY_VAR=hello\napi: echo $MY_VAR";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("MY_VAR"), Some(&"hello".to_string()));
        assert_eq!(procfile.processes.len(), 1);
    }

    #[test]
    fn test_global_env_single_quoted() {
        let input = "MY_VAR='hello world'\napi: echo $MY_VAR";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("MY_VAR"), Some(&"hello world".to_string()));
    }

    #[test]
    fn test_global_env_double_quoted() {
        let input = "MY_VAR=\"hello world\"\napi: echo $MY_VAR";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("MY_VAR"), Some(&"hello world".to_string()));
    }

    #[test]
    fn test_global_env_double_quoted_escapes() {
        let input = "MY_VAR=\"hello\\nworld\"\napi: echo $MY_VAR";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("MY_VAR"), Some(&"hello\nworld".to_string()));
    }

    #[test]
    fn test_global_env_multiple() {
        let input = "FOO=bar\nBAZ=qux\napi: cmd";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(procfile.global_env.get("BAZ"), Some(&"qux".to_string()));
    }

    #[test]
    fn test_global_env_with_underscore() {
        let input = "_MY_VAR=value\napi: cmd";
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("_MY_VAR"), Some(&"value".to_string()));
    }

    #[test]
    fn test_global_env_mixed_with_processes() {
        let input = r#"
CGO_ENABLED=0
NODE_ENV=development

api: go run ./cmd/api
frontend: npm run dev
"#;
        let procfile = parse(input).unwrap();
        assert_eq!(procfile.global_env.get("CGO_ENABLED"), Some(&"0".to_string()));
        assert_eq!(procfile.global_env.get("NODE_ENV"), Some(&"development".to_string()));
        assert_eq!(procfile.processes.len(), 2);
    }

    #[test]
    fn test_global_env_not_confused_with_process() {
        // Lines with colons are processes, not env vars
        let input = "api: MY_VAR=value cmd";
        let procfile = parse(input).unwrap();
        assert!(procfile.global_env.is_empty());
        assert_eq!(procfile.processes.len(), 1);
        assert_eq!(procfile.processes[0].command, "MY_VAR=value cmd");
    }
}
