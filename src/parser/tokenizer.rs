use winnow::combinator::{alt, cut_err, delimited, opt, peek, preceded, repeat, separated_pair, trace};
use winnow::prelude::*;
use winnow::token::{any, none_of, one_of, take_while};

fn bare_word(input: &mut &str) -> ModalResult<String> {
    trace(
        "bare_word",
        take_while(1.., |c: char| {
            c.is_ascii_alphanumeric()
                || c == '_'
                || c == '-'
                || c == '/'
                || c == '.'
                || c == '*'
                || c == '?'
                || c == '['
                || c == ']'
                || c == '{'
                || c == '}'
                || c == '!'
                || c == ','
        })
        .map(|s: &str| s.to_string()),
    )
    .parse_next(input)
}

fn single_quoted(input: &mut &str) -> ModalResult<String> {
    trace(
        "single_quoted",
        delimited('\'', take_while(0.., |c| c != '\''), '\'').map(|s: &str| s.to_string()),
    )
    .parse_next(input)
}

fn double_quoted_escape(input: &mut &str) -> ModalResult<char> {
    preceded(
        '\\',
        alt((
            '\\'.value('\\'),
            '"'.value('"'),
            'n'.value('\n'),
            't'.value('\t'),
            'r'.value('\r'),
        )),
    )
    .parse_next(input)
}

fn double_quoted(input: &mut &str) -> ModalResult<String> {
    trace(
        "double_quoted",
        delimited('"', repeat(0.., alt((double_quoted_escape, none_of(['\\', '"'])))), '"')
            .map(|chars: Vec<char>| chars.into_iter().collect()),
    )
    .parse_next(input)
}

/// A token component that doesn't include colons
fn token_no_colon(input: &mut &str) -> ModalResult<String> {
    alt((
        single_quoted,
        double_quoted,
        bare_word,
        "=".map(|s: &str| s.to_string()),
    ))
    .parse_next(input)
}

/// Check if we're at a line separator (colon followed by whitespace or EOF)
fn at_line_separator(input: &mut &str) -> ModalResult<()> {
    peek((
        ':',
        alt((one_of([' ', '\t', '\n', '\r']).void(), winnow::combinator::eof.void())),
    ))
    .void()
    .parse_next(input)
}

/// Parse a colon that is NOT a line separator (e.g., in "http:8080")
fn embedded_colon(input: &mut &str) -> ModalResult<char> {
    winnow::combinator::not(at_line_separator).parse_next(input)?;
    ':'.parse_next(input)
}

/// Parse a single token, which may contain embedded colons (like "http:8080/health")
/// Also allows trailing colons (like "exec:" when followed by line separator)
fn token_with_colon(input: &mut &str) -> ModalResult<String> {
    trace("token_with_colon", |input: &mut &str| {
        let mut result = String::new();

        loop {
            // Try to parse a regular token part
            if let Ok(part) = token_no_colon(input) {
                result.push_str(&part);
            }

            // Try to consume an embedded colon
            if let Ok(c) = embedded_colon(input) {
                result.push(c);
            } else {
                break;
            }
        }

        if result.is_empty() {
            Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new()))
        } else {
            Ok(result)
        }
    })
    .parse_next(input)
}

/// Skip horizontal whitespace (spaces and tabs)
fn horizontal_space(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c == ' ' || c == '\t')
        .void()
        .parse_next(input)
}

/// Parse all declaration tokens before the colon separator
pub fn tokenize_before_colon(input: &mut &str) -> ModalResult<Vec<String>> {
    trace("tokenize_before_colon", |input: &mut &str| {
        let mut tokens = Vec::new();

        loop {
            horizontal_space(input)?;
            if input.is_empty() || at_line_separator(input).is_ok() {
                break;
            }
            tokens.push(token_with_colon(input)?);
        }

        Ok(tokens)
    })
    .parse_next(input)
}

/// Skip the colon line separator
pub fn skip_colon(input: &mut &str) -> ModalResult<()> {
    ':'.void().parse_next(input)
}

/// Parse an environment variable name (starts with letter or underscore)
fn env_var_name(input: &mut &str) -> ModalResult<String> {
    trace(
        "env_var_name",
        (
            take_while(1, |c: char| c.is_ascii_alphabetic() || c == '_'),
            take_while(0.., |c: char| c.is_ascii_alphanumeric() || c == '_'),
        )
            .map(|(first, rest): (&str, &str)| format!("{}{}", first, rest)),
    )
    .parse_next(input)
}

/// Parse an environment variable value (quoted or bare)
fn env_var_value(input: &mut &str) -> ModalResult<String> {
    alt((
        single_quoted,
        double_quoted,
        take_while(0.., |c: char| !c.is_whitespace()).map(|s: &str| s.to_string()),
    ))
    .parse_next(input)
}

/// Parse a global environment variable assignment (KEY=value)
fn global_env_assignment(input: &mut &str) -> ModalResult<(String, String)> {
    trace(
        "global_env_assignment",
        separated_pair(env_var_name, '=', env_var_value),
    )
    .parse_next(input)
}

// --- Procfile-level parsers ---

/// Represents a single item in the procfile
#[derive(Debug, Clone, PartialEq)]
pub enum ProcfileItem {
    GlobalEnv { key: String, value: String },
    Process(ProcessLine),
}

/// A parsed process line (declaration + command)
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessLine {
    pub declaration_tokens: Vec<String>,
    pub command: String,
}

/// Parse a newline (LF or CRLF)
fn newline(input: &mut &str) -> ModalResult<()> {
    alt(("\r\n", "\n")).void().parse_next(input)
}

/// Parse a comment line (# followed by anything until newline or EOF)
fn comment_line(input: &mut &str) -> ModalResult<()> {
    trace(
        "comment_line",
        (
            horizontal_space,
            '#',
            take_while(0.., |c: char| c != '\n' && c != '\r'),
            opt(newline),
        )
            .void(),
    )
    .parse_next(input)
}

/// Parse a blank line (only whitespace until newline)
fn blank_line(input: &mut &str) -> ModalResult<()> {
    trace("blank_line", (horizontal_space, newline).void()).parse_next(input)
}

/// Parse a backslash-newline continuation sequence
fn line_continuation(input: &mut &str) -> ModalResult<()> {
    ('\\', newline, horizontal_space).void().parse_next(input)
}

/// Parse content up to end of line, handling backslash continuations
fn continued_line_content(input: &mut &str) -> ModalResult<String> {
    trace("continued_line_content", |input: &mut &str| {
        let mut result = String::new();

        loop {
            // Take characters until newline, carriage return, or backslash
            let chunk: &str = take_while(0.., |c: char| c != '\n' && c != '\r' && c != '\\').parse_next(input)?;
            result.push_str(chunk);

            // Try line continuation first
            if line_continuation(input).is_ok() {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                continue;
            }

            // Check for literal backslash (not followed by newline)
            if peek(opt('\\')).parse_next(input)?.is_some_and(|c| c == '\\') {
                let _: char = any.parse_next(input)?;
                result.push('\\');
                continue;
            }

            // End of line or end of input
            break;
        }

        Ok(result)
    })
    .parse_next(input)
}

/// Parse an indented line (for multiline command blocks)
fn indented_line(input: &mut &str) -> ModalResult<String> {
    preceded(
        peek(one_of([' ', '\t'])), // Must start with indent
        continued_line_content,
    )
    .parse_next(input)
}

/// Parse a multiline command block (indented lines after colon-newline)
fn multiline_command_block(input: &mut &str) -> ModalResult<String> {
    trace("multiline_command_block", |input: &mut &str| {
        let mut lines: Vec<String> = Vec::new();

        loop {
            // Check for blank line within block
            if peek(alt((newline.void(), (horizontal_space, newline).void())))
                .parse_next(input)
                .is_ok()
                && (horizontal_space, newline).parse_next(input).is_ok()
            {
                lines.push(String::new());
                continue;
            }

            // Try to parse an indented line
            if let Ok(line) = indented_line(input) {
                lines.push(line);
                let _ = opt(newline).parse_next(input)?;
            } else {
                break;
            }
        }

        // Remove trailing empty lines
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }

        if lines.is_empty() {
            return Ok(String::new());
        }

        // Find minimum indentation
        let min_indent = lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);

        // Strip common indentation and join
        let command = lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    ""
                } else if l.len() > min_indent {
                    &l[min_indent..]
                } else {
                    l.trim_start()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(command)
    })
    .parse_next(input)
}

/// Parse the command part after the colon
fn command_part(input: &mut &str) -> ModalResult<String> {
    trace("command_part", |input: &mut &str| {
        horizontal_space(input)?;

        // Check if this is a multiline block (colon followed immediately by newline)
        if peek(alt((newline.void(), winnow::combinator::eof.void())))
            .parse_next(input)
            .is_ok()
        {
            if newline(input).is_ok() {
                return multiline_command_block(input);
            }
            return Ok(String::new());
        }

        // Inline command with possible continuation
        let cmd = continued_line_content(input)?;
        let _ = opt(newline).parse_next(input)?;
        Ok(cmd)
    })
    .parse_next(input)
}

/// Parse a process definition line
fn process_line(input: &mut &str) -> ModalResult<ProcessLine> {
    trace("process_line", |input: &mut &str| {
        horizontal_space(input)?;

        let declaration_tokens = tokenize_before_colon(input)?;

        if declaration_tokens.is_empty() {
            return Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new()));
        }

        // After seeing tokens, commit to this being a process line
        skip_colon(input)?;
        let command = cut_err(command_part)
            .context(winnow::error::StrContext::Label("command"))
            .parse_next(input)?;

        Ok(ProcessLine {
            declaration_tokens,
            command,
        })
    })
    .parse_next(input)
}

/// Parse a global environment variable line (no colon on the line)
fn global_env_line(input: &mut &str) -> ModalResult<ProcfileItem> {
    trace("global_env_line", |input: &mut &str| {
        horizontal_space(input)?;

        let (key, value) = global_env_assignment(input)?;

        // Must be at end of line or EOF (no colon means not a process)
        horizontal_space(input)?;

        // Verify we're at EOL or EOF
        peek(alt((newline.void(), winnow::combinator::eof.void()))).parse_next(input)?;
        let _ = opt(newline).parse_next(input)?;

        Ok(ProcfileItem::GlobalEnv { key, value })
    })
    .parse_next(input)
}

/// Parse a single procfile item (comment/blank/env/process)
fn procfile_item(input: &mut &str) -> ModalResult<Option<ProcfileItem>> {
    trace(
        "procfile_item",
        alt((
            blank_line.map(|_| None),
            comment_line.map(|_| None),
            global_env_line.map(Some),
            process_line.map(|p| Some(ProcfileItem::Process(p))),
        )),
    )
    .parse_next(input)
}

/// Parse the entire procfile into items
pub fn parse_procfile(input: &mut &str) -> ModalResult<Vec<ProcfileItem>> {
    trace("parse_procfile", |input: &mut &str| {
        let items: Vec<Option<ProcfileItem>> = repeat(0.., procfile_item).parse_next(input)?;
        Ok(items.into_iter().flatten().collect())
    })
    .parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_quoted() {
        let mut input = "'hello world'";
        assert_eq!(single_quoted(&mut input).unwrap(), "hello world");
    }

    #[test]
    fn test_double_quoted() {
        let mut input = r#""hello\nworld""#;
        assert_eq!(double_quoted(&mut input).unwrap(), "hello\nworld");
    }

    #[test]
    fn test_double_quoted_with_escaped_quote() {
        let mut input = r#""say \"hi\"""#;
        assert_eq!(double_quoted(&mut input).unwrap(), "say \"hi\"");
    }

    #[test]
    fn test_tokenize_before_colon() {
        let mut input = "api **/*.go after=db: go run ./cmd";
        let tokens = tokenize_before_colon(&mut input).unwrap();
        assert_eq!(tokens, vec!["api", "**/*.go", "after", "=", "db"]);
        assert!(input.starts_with(':'));
    }

    #[test]
    fn test_glob_not_confused_with_key_value() {
        let mut input = "api src/*.go: cmd";
        let tokens = tokenize_before_colon(&mut input).unwrap();
        assert_eq!(tokens, vec!["api", "src/*.go"]);
    }

    #[test]
    fn test_token_with_embedded_colon() {
        let mut input = "ready=http:8080/health: cmd";
        let tokens = tokenize_before_colon(&mut input).unwrap();
        assert_eq!(tokens, vec!["ready", "=", "http:8080/health"]);
    }

    #[test]
    fn test_token_with_trailing_colon() {
        // exec: followed by line separator should produce "exec:" as token
        let mut input = "ready=exec:: ./api";
        let tokens = tokenize_before_colon(&mut input).unwrap();
        assert_eq!(tokens, vec!["ready", "=", "exec:"]);
    }

    #[test]
    fn test_global_env_line() {
        let mut input = "FOO=bar\n";
        let item = global_env_line(&mut input).unwrap();
        assert_eq!(
            item,
            ProcfileItem::GlobalEnv {
                key: "FOO".to_string(),
                value: "bar".to_string()
            }
        );
    }

    #[test]
    fn test_process_line_simple() {
        let mut input = "api: go run ./cmd/api\n";
        let proc = process_line(&mut input).unwrap();
        assert_eq!(proc.declaration_tokens, vec!["api"]);
        assert_eq!(proc.command, "go run ./cmd/api");
    }

    #[test]
    fn test_multiline_block() {
        let mut input = "api:\n    echo hello\n    echo world\n";
        let proc = process_line(&mut input).unwrap();
        assert_eq!(proc.command, "echo hello\necho world");
    }

    #[test]
    fn test_line_continuation() {
        let mut input = "api: go run \\\n  ./cmd/api\n";
        let proc = process_line(&mut input).unwrap();
        assert_eq!(proc.command, "go run ./cmd/api");
    }
}
