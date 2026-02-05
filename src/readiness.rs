use crate::parser::ReadyProbe;
use std::collections::HashMap;
use std::net::TcpStream;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessResult {
    Ready,
    TimedOut,
}

/// Polls a readiness probe until it succeeds or times out.
/// Returns `Ready` if the probe succeeds within the timeout, `TimedOut` otherwise.
#[allow(dead_code)]
pub fn wait_for_ready(probe: &ReadyProbe, env: &HashMap<String, String>) -> ReadinessResult {
    let start = Instant::now();

    while start.elapsed() < PROBE_TIMEOUT {
        if check_probe(probe, env) {
            return ReadinessResult::Ready;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    ReadinessResult::TimedOut
}

/// Checks a probe once, returning true if ready.
fn check_probe(probe: &ReadyProbe, env: &HashMap<String, String>) -> bool {
    match probe {
        ReadyProbe::Tcp { port } => check_tcp(*port),
        ReadyProbe::Http {
            port,
            path,
            expected_status,
        } => check_http(*port, path, *expected_status),
        ReadyProbe::Exec { command } => check_exec(command, env),
    }
}

/// Attempts a TCP connection to localhost:port (tries both IPv4 and IPv6).
fn check_tcp(port: u16) -> bool {
    let addrs = [format!("127.0.0.1:{}", port), format!("[::1]:{}", port)];
    addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(&addr.parse().unwrap(), CONNECT_TIMEOUT).is_ok())
}

/// Attempts an HTTP GET to localhost:port/path.
/// If expected_status is Some, returns true only if status matches exactly.
/// Otherwise returns true for any non-5xx response.
/// Tries both IPv4 and IPv6 loopback addresses.
fn check_http(port: u16, path: &str, expected_status: Option<u16>) -> bool {
    let addrs = [format!("127.0.0.1:{}", port), format!("[::1]:{}", port)];
    addrs
        .iter()
        .any(|addr| check_http_addr(addr, port, path, expected_status))
}

fn check_http_addr(addr: &str, port: u16, path: &str, expected_status: Option<u16>) -> bool {
    // Build a minimal HTTP/1.0 request
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    };
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        path, port
    );

    let stream = match TcpStream::connect_timeout(&addr.parse().unwrap(), CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return false,
    };

    if stream.set_read_timeout(Some(CONNECT_TIMEOUT)).is_err() {
        return false;
    }
    if stream.set_write_timeout(Some(CONNECT_TIMEOUT)).is_err() {
        return false;
    }

    use std::io::{Read, Write};
    let mut stream = stream;

    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut response = [0u8; 256];
    let n = match stream.read(&mut response) {
        Ok(n) => n,
        Err(_) => return false,
    };

    let response_str = String::from_utf8_lossy(&response[..n]);
    match (parse_http_status(&response_str), expected_status) {
        (Some(code), Some(expected)) => code == expected,
        (Some(code), None) => code < 500,
        (None, _) => false,
    }
}

/// Executes a command via the shell and returns true if it exits with code 0.
fn check_exec(command: &str, env: &HashMap<String, String>) -> bool {
    use std::process::Command;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut cmd = Command::new(&shell);
    cmd.arg("-c").arg(command);
    cmd.envs(env);

    match cmd.output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Parses the status code from an HTTP response.
fn parse_http_status(response: &str) -> Option<u16> {
    // HTTP/1.x STATUS_CODE REASON
    let first_line = response.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _version = parts.next()?;
    let status = parts.next()?;
    status.parse().ok()
}

/// Non-blocking check if a probe is ready (single attempt).
#[allow(dead_code)]
pub fn is_ready(probe: &ReadyProbe, env: &HashMap<String, String>) -> bool {
    check_probe(probe, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_tcp_probe_success() {
        // Start a listener on a random port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept in background so connection succeeds
        let handle = thread::spawn(move || {
            let _ = listener.accept();
        });

        assert!(check_tcp(port));
        handle.join().unwrap();
    }

    #[test]
    fn test_tcp_probe_failure() {
        // Use a port that's very unlikely to be open
        assert!(!check_tcp(59999));
    }

    #[test]
    fn test_http_probe_success() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(check_http(port, "/health", None));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_5xx_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(!check_http(port, "/", None));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_4xx_accepted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(check_http(port, "/health", None));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_failure() {
        assert!(!check_http(59998, "/", None));
    }

    #[test]
    fn test_parse_http_status() {
        assert_eq!(parse_http_status("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_http_status("HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_http_status("HTTP/1.1 503 Service Unavailable\r\n"), Some(503));
        assert_eq!(parse_http_status("garbage"), None);
    }

    #[test]
    fn test_wait_for_ready_immediate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            let _ = listener.accept();
        });

        let probe = ReadyProbe::Tcp { port };
        let result = wait_for_ready(&probe, &HashMap::new());
        assert_eq!(result, ReadinessResult::Ready);

        handle.join().unwrap();
    }

    #[test]
    fn test_tcp_probe_ipv6_success() {
        // Start a listener on IPv6 loopback
        let listener = match TcpListener::bind("[::1]:0") {
            Ok(l) => l,
            Err(_) => return, // Skip if IPv6 not available
        };
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            let _ = listener.accept();
        });

        assert!(check_tcp(port));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_ipv6_success() {
        // Start a listener on IPv6 loopback
        let listener = match TcpListener::bind("[::1]:0") {
            Ok(l) => l,
            Err(_) => return, // Skip if IPv6 not available
        };
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(check_http(port, "/health", None));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_exact_status_match() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 201 Created\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(check_http(port, "/", Some(201)));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_exact_status_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(!check_http(port, "/", Some(201)));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_exact_status_5xx() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        // With exact status, even 5xx can match if explicitly expected
        assert!(check_http(port, "/", Some(503)));
        handle.join().unwrap();
    }

    #[test]
    fn test_exec_probe_success() {
        assert!(check_exec("true", &HashMap::new()));
    }

    #[test]
    fn test_exec_probe_failure() {
        assert!(!check_exec("false", &HashMap::new()));
    }

    #[test]
    fn test_exec_probe_with_command() {
        assert!(check_exec("test 1 -eq 1", &HashMap::new()));
    }

    #[test]
    fn test_exec_probe_with_failing_command() {
        assert!(!check_exec("test 1 -eq 2", &HashMap::new()));
    }

    #[test]
    fn test_exec_probe_with_env() {
        let mut env = HashMap::new();
        env.insert("PROBE_TEST_VAR".to_string(), "expected_value".to_string());
        assert!(check_exec("test \"$PROBE_TEST_VAR\" = expected_value", &env));
    }

    #[test]
    fn test_exec_probe_without_env_fails() {
        assert!(!check_exec(
            "test \"$PROBE_TEST_MISSING\" = expected_value",
            &HashMap::new()
        ));
    }
}
