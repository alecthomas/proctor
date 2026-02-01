use crate::orchestrator::runner::{OutputLine, OutputSource};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use yansi::{Color, Paint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    Ready,
    Finished,
    Stopped,
    Crashed,
    Restarting,
}

impl ControlEvent {
    fn symbol(&self) -> &'static str {
        match self {
            ControlEvent::Ready => "●",
            ControlEvent::Finished => "✔",
            ControlEvent::Stopped => "☠",
            ControlEvent::Crashed => "↯",
            ControlEvent::Restarting => "↻",
        }
    }

    fn color(&self) -> Color {
        match self {
            ControlEvent::Ready => Color::Green,
            ControlEvent::Finished => Color::Green,
            ControlEvent::Stopped => Color::Red,
            ControlEvent::Crashed => Color::Red,
            ControlEvent::Restarting => Color::Yellow,
        }
    }
}

/// Selects a color from the 256-color palette based on a hash of the name.
/// Excludes colors that are too dark (0-16) or too light (grayscale 232-255).
fn color_for_name(name: &str) -> Color {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = hasher.finish();

    // Use colors 17-231 (the 6x6x6 color cube), avoiding problematic colors
    let usable_colors: Vec<u8> = (17u8..=231)
        .filter(|&c| {
            let idx = c - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            let sum = r + g + b;
            // Exclude very dark (sum < 4) and very light (sum > 11)
            if sum < 4 || sum > 11 {
                return false;
            }
            // Exclude reddish colors (r dominant) - they look like errors
            if r >= 3 && r > g && r > b {
                return false;
            }
            // Exclude dark blues/purples (b dominant with low g)
            if b >= 3 && g <= 1 && r <= 1 {
                return false;
            }
            true
        })
        .collect();

    let idx = (hash as usize) % usable_colors.len();
    Color::Fixed(usable_colors[idx])
}

pub struct OutputFormatter {
    max_name_len: usize,
}

impl OutputFormatter {
    pub fn new(process_names: &[&str]) -> Self {
        let max_name_len = process_names.iter().map(|n| n.len()).max().unwrap_or(0);
        Self { max_name_len }
    }

    pub fn format(&self, line: &OutputLine) -> String {
        let color = color_for_name(&line.process);
        let prefix = format!("{:>width$} │", line.process, width = self.max_name_len);

        let styled_prefix = prefix.paint(color);

        match line.source {
            OutputSource::Stdout => format!("{} {}", styled_prefix, line.content),
            OutputSource::Stderr => format!("{} {}", styled_prefix, line.content.dim().italic()),
        }
    }

    pub fn format_control(&self, process: &str, event: ControlEvent, message: &str) -> String {
        let process_color = color_for_name(process);
        let prefix = format!(
            "{:>width$} {}",
            process,
            event.symbol(),
            width = self.max_name_len
        );
        let styled_prefix = prefix.paint(process_color);
        let styled_message = message.paint(event.color());

        format!("{} {}", styled_prefix, styled_message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_deterministic() {
        let c1 = color_for_name("api");
        let c2 = color_for_name("api");
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_color_different_names() {
        let c1 = color_for_name("api");
        let c2 = color_for_name("worker");
        // Different names should (usually) get different colors
        // This isn't guaranteed but is highly likely
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_prefix_alignment() {
        let formatter = OutputFormatter::new(&["api", "worker", "frontend"]);

        let line = OutputLine {
            process: "api".to_string(),
            source: OutputSource::Stdout,
            content: "hello".to_string(),
        };

        let output = formatter.format(&line);
        // "frontend" is 8 chars, so "api" should be padded to 8
        // The output contains ANSI codes, so we check the structure
        assert!(output.contains("api"));
        assert!(output.contains("│"));
        assert!(output.contains("hello"));
    }

    #[test]
    fn test_stderr_styling() {
        let formatter = OutputFormatter::new(&["test"]);

        let stdout_line = OutputLine {
            process: "test".to_string(),
            source: OutputSource::Stdout,
            content: "out".to_string(),
        };

        let stderr_line = OutputLine {
            process: "test".to_string(),
            source: OutputSource::Stderr,
            content: "err".to_string(),
        };

        let stdout_output = formatter.format(&stdout_line);
        let stderr_output = formatter.format(&stderr_line);

        // Both should contain the content
        assert!(stdout_output.contains("out"));
        assert!(stderr_output.contains("err"));

        // Stderr should have different styling (dim/italic adds more ANSI codes)
        assert_ne!(stdout_output.len(), stderr_output.len());
    }
}
