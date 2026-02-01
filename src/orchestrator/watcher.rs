use crate::parser::GlobPattern;
use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ReloadEvent {
    pub process: String,
    pub path: String,
}

struct ProcessMatcher {
    name: String,
    includes: GlobSet,
    excludes: GlobSet,
}

impl ProcessMatcher {
    fn matches(&self, path: &Path) -> bool {
        if self.includes.is_empty() {
            return false;
        }
        let matches_include = self.includes.is_match(path);
        let matches_exclude = self.excludes.is_match(path);
        matches_include && !matches_exclude
    }
}

pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<ReloadEvent>,
}

impl FileWatcher {
    pub fn new(
        base_dir: &Path,
        processes: Vec<(String, Vec<GlobPattern>, Duration)>,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();

        let matchers = build_matchers(processes)?;
        let base_dir_owned = base_dir.to_path_buf();
        let base_dir_for_watch = base_dir.to_path_buf();

        let event_tx = tx.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    handle_event(&event, &base_dir_owned, &matchers, &event_tx);
                }
            },
            notify::Config::default(),
        )
        .map_err(|e| e.to_string())?;

        watcher
            .watch(&base_dir_for_watch, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;

        Ok(Self {
            _watcher: watcher,
            receiver: rx,
        })
    }

    pub fn try_recv(&self) -> Option<ReloadEvent> {
        self.receiver.try_recv().ok()
    }
}

fn normalize_pattern(pattern: &str) -> &str {
    pattern.strip_prefix("./").unwrap_or(pattern)
}

fn build_matchers(
    processes: Vec<(String, Vec<GlobPattern>, Duration)>,
) -> Result<Vec<ProcessMatcher>, String> {
    let mut matchers = Vec::new();

    for (name, patterns, _debounce) in processes {
        let mut includes = GlobSetBuilder::new();
        let mut excludes = GlobSetBuilder::new();

        for pattern in patterns {
            let normalized = normalize_pattern(&pattern.pattern);
            let glob = Glob::new(normalized)
                .map_err(|e| format!("invalid glob pattern '{}': {}", pattern.pattern, e))?;

            if pattern.exclude {
                excludes.add(glob);
            } else {
                includes.add(glob);
            }
        }

        let includes = includes.build().map_err(|e| e.to_string())?;
        let excludes = excludes.build().map_err(|e| e.to_string())?;

        matchers.push(ProcessMatcher {
            name,
            includes,
            excludes,
        });
    }

    Ok(matchers)
}

fn handle_event(
    event: &Event,
    base_dir: &Path,
    matchers: &[ProcessMatcher],
    tx: &Sender<ReloadEvent>,
) {
    use notify::EventKind;

    // Only handle create, modify, and remove events
    let dominated = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    );

    if !dominated {
        return;
    }

    // Skip metadata-only changes
    if matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(_))) {
        return;
    }

    let mut triggered: HashMap<String, String> = HashMap::new();

    for path in &event.paths {
        let relative = match path.strip_prefix(base_dir) {
            Ok(r) => r,
            Err(_) => path.as_path(),
        };

        for matcher in matchers {
            if triggered.contains_key(&matcher.name) {
                continue;
            }

            if matcher.matches(relative) {
                triggered.insert(matcher.name.clone(), relative.display().to_string());
            }
        }
    }

    for (name, path) in triggered {
        let _ = tx.send(ReloadEvent {
            process: name,
            path,
        });
    }
}

pub struct Debouncer {
    pending: HashMap<String, (Instant, String)>,
    debounce_durations: HashMap<String, Duration>,
}

impl Default for Debouncer {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            debounce_durations: HashMap::new(),
        }
    }
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_debounce(&mut self, process: &str, duration: Duration) {
        self.debounce_durations
            .insert(process.to_string(), duration);
    }

    pub fn record_event(&mut self, process: &str, path: &str) {
        self.pending
            .insert(process.to_string(), (Instant::now(), path.to_string()));
    }

    pub fn get_ready(&mut self) -> Vec<(String, String)> {
        let now = Instant::now();
        let mut ready = Vec::new();

        self.pending.retain(|name, (instant, path)| {
            let debounce = self
                .debounce_durations
                .get(name)
                .copied()
                .unwrap_or(Duration::from_millis(500));

            if now.duration_since(*instant) >= debounce {
                ready.push((name.clone(), path.clone()));
                false
            } else {
                true
            }
        });

        ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(pattern: &str, exclude: bool) -> GlobPattern {
        GlobPattern {
            pattern: pattern.to_string(),
            exclude,
        }
    }

    #[test]
    fn test_build_matchers_empty() {
        let matchers = build_matchers(vec![]).unwrap();
        assert!(matchers.is_empty());
    }

    #[test]
    fn test_build_matchers_simple() {
        let matchers = build_matchers(vec![(
            "api".to_string(),
            vec![glob("**/*.go", false)],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0].name, "api");
    }

    #[test]
    fn test_matcher_matches_include() {
        let matchers = build_matchers(vec![(
            "api".to_string(),
            vec![glob("**/*.go", false)],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert!(matchers[0].matches(Path::new("cmd/main.go")));
        assert!(matchers[0].matches(Path::new("pkg/api/handler.go")));
        assert!(!matchers[0].matches(Path::new("README.md")));
    }

    #[test]
    fn test_matcher_matches_exclude() {
        let matchers = build_matchers(vec![(
            "api".to_string(),
            vec![glob("**/*.go", false), glob("**/*_test.go", true)],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert!(matchers[0].matches(Path::new("cmd/main.go")));
        assert!(!matchers[0].matches(Path::new("cmd/main_test.go")));
    }

    #[test]
    fn test_matcher_no_patterns() {
        let matchers = build_matchers(vec![(
            "api".to_string(),
            vec![],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert!(!matchers[0].matches(Path::new("anything.go")));
    }

    #[test]
    fn test_debouncer_immediate() {
        let mut debouncer = Debouncer::new();
        debouncer.set_debounce("api", Duration::from_millis(0));
        debouncer.record_event("api", "main.go");

        let ready = debouncer.get_ready();
        assert_eq!(ready, vec![("api".to_string(), "main.go".to_string())]);
    }

    #[test]
    fn test_debouncer_pending() {
        let mut debouncer = Debouncer::new();
        debouncer.set_debounce("api", Duration::from_secs(10));
        debouncer.record_event("api", "main.go");

        let ready = debouncer.get_ready();
        assert!(ready.is_empty());
    }

    #[test]
    fn test_debouncer_multiple_events() {
        let mut debouncer = Debouncer::new();
        debouncer.set_debounce("api", Duration::from_millis(0));
        debouncer.set_debounce("worker", Duration::from_millis(0));

        debouncer.record_event("api", "main.go");
        debouncer.record_event("worker", "worker.go");

        let mut ready = debouncer.get_ready();
        ready.sort();
        assert_eq!(
            ready,
            vec![
                ("api".to_string(), "main.go".to_string()),
                ("worker".to_string(), "worker.go".to_string())
            ]
        );
    }

    #[test]
    fn test_matcher_vendor_exclusion() {
        let matchers = build_matchers(vec![(
            "api".to_string(),
            vec![glob("**/*.go", false), glob("vendor/**", true)],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert!(matchers[0].matches(Path::new("main.go")));
        assert!(matchers[0].matches(Path::new("pkg/api.go")));
        assert!(!matchers[0].matches(Path::new("vendor/lib/lib.go")));
    }

    #[test]
    fn test_matcher_multiple_extensions() {
        let matchers = build_matchers(vec![(
            "frontend".to_string(),
            vec![
                glob("**/*.ts", false),
                glob("**/*.tsx", false),
                glob("**/*.css", false),
            ],
            Duration::from_millis(500),
        )])
        .unwrap();

        assert!(matchers[0].matches(Path::new("src/app.ts")));
        assert!(matchers[0].matches(Path::new("components/Button.tsx")));
        assert!(matchers[0].matches(Path::new("styles/main.css")));
        assert!(!matchers[0].matches(Path::new("README.md")));
    }

    #[test]
    fn test_matcher_exact_file() {
        let matchers = build_matchers(vec![(
            "echo".to_string(),
            vec![glob("./test.txt", false)],
            Duration::from_millis(500),
        )])
        .unwrap();

        // Pattern ./test.txt is normalized to test.txt
        assert!(matchers[0].matches(Path::new("test.txt")));
    }

    #[test]
    fn test_debouncer_updates_timestamp() {
        let mut debouncer = Debouncer::new();
        debouncer.set_debounce("api", Duration::from_millis(50));

        debouncer.record_event("api", "main.go");
        std::thread::sleep(Duration::from_millis(30));

        // Event not ready yet
        assert!(debouncer.get_ready().is_empty());

        // Record another event, resetting the debounce timer
        debouncer.record_event("api", "handler.go");
        std::thread::sleep(Duration::from_millis(30));

        // Still not ready because timer was reset
        assert!(debouncer.get_ready().is_empty());

        // Wait for debounce to complete
        std::thread::sleep(Duration::from_millis(30));
        // The path should be the last one recorded
        assert_eq!(
            debouncer.get_ready(),
            vec![("api".to_string(), "handler.go".to_string())]
        );
    }
}
