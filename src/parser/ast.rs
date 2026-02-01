use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Procfile {
    pub processes: Vec<ProcessDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDef {
    pub name: String,
    pub watch_patterns: Vec<GlobPattern>,
    pub options: ProcessOptions,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobPattern {
    pub pattern: String,
    pub exclude: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOptions {
    pub after: Vec<String>,
    pub ready: Option<ReadyProbe>,
    pub signal: Signal,
    pub debounce: Duration,
    pub dir: Option<String>,
    pub shutdown: Duration,
}

impl Default for ProcessOptions {
    fn default() -> Self {
        Self {
            after: Vec::new(),
            ready: None,
            signal: Signal::Term,
            debounce: Duration::from_millis(500),
            dir: None,
            shutdown: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyProbe {
    Tcp { port: u16 },
    Http { port: u16, path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Signal {
    Hup,
    Int,
    #[default]
    Term,
    Kill,
    Usr1,
    Usr2,
}

impl Signal {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "HUP" | "SIGHUP" => Some(Signal::Hup),
            "INT" | "SIGINT" => Some(Signal::Int),
            "TERM" | "SIGTERM" => Some(Signal::Term),
            "KILL" | "SIGKILL" => Some(Signal::Kill),
            "USR1" | "SIGUSR1" => Some(Signal::Usr1),
            "USR2" | "SIGUSR2" => Some(Signal::Usr2),
            _ => None,
        }
    }

    pub fn to_nix(self) -> nix::sys::signal::Signal {
        match self {
            Signal::Hup => nix::sys::signal::Signal::SIGHUP,
            Signal::Int => nix::sys::signal::Signal::SIGINT,
            Signal::Term => nix::sys::signal::Signal::SIGTERM,
            Signal::Kill => nix::sys::signal::Signal::SIGKILL,
            Signal::Usr1 => nix::sys::signal::Signal::SIGUSR1,
            Signal::Usr2 => nix::sys::signal::Signal::SIGUSR2,
        }
    }
}
