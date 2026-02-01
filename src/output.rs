use crate::orchestrator::runner::{OutputLine, OutputSource};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use yansi::{Color, Paint};

/// Selects a color from the 256-color palette based on a hash of the name.
/// Excludes colors that are too dark (0-16) or too light (grayscale 232-255).
fn color_for_name(name: &str) -> Color {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = hasher.finish();

    // Use colors 17-231 (the 6x6x6 color cube), avoiding the darkest and lightest
    // Skip colors where all RGB components are 0 or 5 (too dark/light)
    let usable_colors: Vec<u8> = (17u8..=231)
        .filter(|&c| {
            let idx = c - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            // Exclude very dark (sum < 3) and very light (sum > 12)
            let sum = r + g + b;
            sum >= 3 && sum <= 12
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
        let prefix = format!("{:>width$} |", line.process, width = self.max_name_len);

        let styled_prefix = match line.source {
            OutputSource::Stdout => prefix.paint(color),
            OutputSource::Stderr => prefix.paint(color).dim().italic(),
        };

        format!("{} {}", styled_prefix, line.content)
    }

    pub fn format_control(&self, process: &str, message: &str) -> String {
        let color = color_for_name(process);
        let prefix = format!("{:>width$} |", process, width = self.max_name_len);
        let styled_prefix = prefix.paint(color).dim();
        let styled_message = message.paint(color).dim();

        format!("{} {}", styled_prefix, styled_message)
    }

    pub fn format_error(&self, process: &str, message: &str) -> String {
        let color = color_for_name(process);
        let prefix = format!("{:>width$} |", process, width = self.max_name_len);
        let styled_prefix = prefix.paint(color).dim();
        let styled_message = message.paint(Color::Red).bold();

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
        assert!(output.contains("|"));
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
