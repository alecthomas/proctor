use crate::parser::ReadyProbe;
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
pub fn wait_for_ready(probe: &ReadyProbe) -> ReadinessResult {
    let start = Instant::now();

    while start.elapsed() < PROBE_TIMEOUT {
        if check_probe(probe) {
            return ReadinessResult::Ready;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    ReadinessResult::TimedOut
}

/// Checks a probe once, returning true if ready.
fn check_probe(probe: &ReadyProbe) -> bool {
    match probe {
        ReadyProbe::Tcp { port } => check_tcp(*port),
        ReadyProbe::Http { port, path } => check_http(*port, path),
    }
}

/// Attempts a TCP connection to localhost:port.
fn check_tcp(port: u16) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    TcpStream::connect_timeout(&addr.parse().unwrap(), CONNECT_TIMEOUT).is_ok()
}

/// Attempts an HTTP GET to localhost:port/path, returns true if 2xx response.
fn check_http(port: u16, path: &str) -> bool {
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

    let addr = format!("127.0.0.1:{}", port);
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

    // Parse HTTP response status line: "HTTP/1.x 2xx ..."
    let response_str = String::from_utf8_lossy(&response[..n]);
    parse_http_status(&response_str)
        .map(|code| (200..300).contains(&code))
        .unwrap_or(false)
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
pub fn is_ready(probe: &ReadyProbe) -> bool {
    check_probe(probe)
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

        assert!(check_http(port, "/health"));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_non_2xx() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let response = "HTTP/1.0 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        assert!(!check_http(port, "/"));
        handle.join().unwrap();
    }

    #[test]
    fn test_http_probe_failure() {
        assert!(!check_http(59998, "/"));
    }

    #[test]
    fn test_parse_http_status() {
        assert_eq!(parse_http_status("HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(parse_http_status("HTTP/1.0 404 Not Found\r\n"), Some(404));
        assert_eq!(
            parse_http_status("HTTP/1.1 503 Service Unavailable\r\n"),
            Some(503)
        );
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
        let result = wait_for_ready(&probe);
        assert_eq!(result, ReadinessResult::Ready);

        handle.join().unwrap();
    }
}
