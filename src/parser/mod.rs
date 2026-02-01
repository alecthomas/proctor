#![allow(dead_code)]

mod ast;
mod tokenizer;

pub use ast::*;

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Procfile, ParseError> {
    let mut processes = Vec::new();
    let mut lines_iter = input.lines().enumerate().peekable();

    while let Some((line_num, line)) = lines_iter.next() {
        let line_num = line_num + 1; // 1-indexed

        // Handle line continuation
        let mut full_line = line.to_string();
        while full_line.ends_with('\\') {
            full_line.pop();
            if let Some((_, next_line)) = lines_iter.next() {
                full_line.push_str(next_line);
            }
        }

        let trimmed = full_line.trim();

        // Skip blank lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let process = parse_line(trimmed, line_num)?;
        processes.push(process);
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

    Ok(Procfile { processes })
}

fn parse_line(line: &str, line_num: usize) -> Result<ProcessDef, ParseError> {
    let mut input = line;

    let decl_tokens = tokenizer::tokenize_before_colon(&mut input).map_err(|e| ParseError {
        line: line_num,
        message: format!("tokenization error: {}", e),
    })?;

    if decl_tokens.is_empty() {
        return Err(ParseError {
            line: line_num,
            message: "missing process name".to_string(),
        });
    }

    tokenizer::skip_colon(&mut input).map_err(|_| ParseError {
        line: line_num,
        message: "missing colon separator".to_string(),
    })?;

    let command_part = tokenizer::rest_of_line(&mut input).map_err(|e| ParseError {
        line: line_num,
        message: format!("failed to parse command: {}", e),
    })?;

    // Parse declaration tokens - check for oneshot suffix (name!)
    let raw_name = &decl_tokens[0];
    let (name, oneshot) = if let Some(n) = raw_name.strip_suffix('!') {
        (n.to_string(), true)
    } else {
        (raw_name.clone(), false)
    };

    if !is_valid_name(&name) {
        return Err(ParseError {
            line: line_num,
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
            let value = &decl_tokens[i + 2];
            apply_option(&mut options, key, value, line_num)?;
            i += 3;
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

    if command_part.is_empty() {
        return Err(ParseError {
            line: line_num,
            message: "missing command".to_string(),
        });
    }

    Ok(ProcessDef {
        name,
        watch_patterns,
        options,
        command: command_part,
        oneshot,
    })
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn apply_option(
    opts: &mut ProcessOptions,
    key: &str,
    value: &str,
    line_num: usize,
) -> Result<(), ParseError> {
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
        let (port_str, path) = if let Some(idx) = rest.find('/') {
            (&rest[..idx], rest[idx..].to_string())
        } else {
            (rest, "/".to_string())
        };
        let port: u16 = port_str.parse().map_err(|_| ParseError {
            line: line_num,
            message: format!("invalid port in ready probe: {}", port_str),
        })?;
        Ok(ReadyProbe::Http { port, path })
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
                    message: format!(
                        "process '{}' depends on unknown process '{}'",
                        proc.name, dep
                    ),
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
        assert_eq!(
            procfile.processes[1].options.debounce,
            Duration::from_secs(1)
        );
    }

    #[test]
    fn test_with_env() {
        let input = "api: CGO_ENABLED=0 go run ./cmd/api";
        let procfile = parse(input).unwrap();
        assert_eq!(
            procfile.processes[0].command,
            "CGO_ENABLED=0 go run ./cmd/api"
        );
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
                path: "/health".to_string()
            })
        );
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
        assert_eq!(
            procfile.processes[0].command,
            "go run   -tags dev   ./cmd/api"
        );
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
}
