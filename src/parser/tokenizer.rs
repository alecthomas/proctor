use winnow::combinator::{alt, delimited, repeat};
use winnow::prelude::*;
use winnow::token::{none_of, take_while};

fn bare_word(input: &mut &str) -> ModalResult<String> {
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
    .map(|s: &str| s.to_string())
    .parse_next(input)
}

fn single_quoted(input: &mut &str) -> ModalResult<String> {
    delimited('\'', take_while(0.., |c| c != '\''), '\'')
        .map(|s: &str| s.to_string())
        .parse_next(input)
}

fn double_quoted_char(input: &mut &str) -> ModalResult<char> {
    alt((
        '\\'.value('\\'),
        '"'.value('"'),
        'n'.value('\n'),
        't'.value('\t'),
        'r'.value('\r'),
    ))
    .parse_next(input)
}

fn double_quoted_escape(input: &mut &str) -> ModalResult<char> {
    ('\\', double_quoted_char).map(|(_, c)| c).parse_next(input)
}

fn double_quoted_content(input: &mut &str) -> ModalResult<char> {
    alt((double_quoted_escape, none_of(['\\', '"']))).parse_next(input)
}

fn double_quoted(input: &mut &str) -> ModalResult<String> {
    delimited('"', repeat(0.., double_quoted_content), '"')
        .map(|chars: Vec<char>| chars.into_iter().collect())
        .parse_next(input)
}

fn token_no_colon(input: &mut &str) -> ModalResult<String> {
    alt((
        single_quoted,
        double_quoted,
        bare_word,
        "=".map(|s: &str| s.to_string()),
    ))
    .parse_next(input)
}

fn is_line_separator(input: &str) -> bool {
    if !input.starts_with(':') {
        return false;
    }
    // A colon is the line separator if followed by whitespace or end-of-input
    input.len() == 1 || input.chars().nth(1).map(|c| c.is_whitespace()).unwrap_or(true)
}

pub fn tokenize_before_colon(input: &mut &str) -> ModalResult<Vec<String>> {
    let mut tokens = Vec::new();
    loop {
        let _ = take_while(0.., |c: char| c.is_whitespace()).parse_next(input)?;
        if input.is_empty() || is_line_separator(input) {
            break;
        }
        tokens.push(token_with_colon(input)?);
    }
    Ok(tokens)
}

fn token_with_colon(input: &mut &str) -> ModalResult<String> {
    let mut result = String::new();
    loop {
        // Try to parse a regular token part
        if let Ok(part) = token_no_colon(input) {
            result.push_str(&part);
        }
        // Check if we should consume a colon (only if not followed by whitespace)
        if input.starts_with(':') && !is_line_separator(input) {
            result.push(':');
            *input = &input[1..];
        } else {
            break;
        }
    }
    if result.is_empty() {
        Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new()))
    } else {
        Ok(result)
    }
}

pub fn skip_colon(input: &mut &str) -> ModalResult<()> {
    ':'.void().parse_next(input)
}

pub fn rest_of_line(input: &mut &str) -> ModalResult<String> {
    let _ = take_while(0.., |c: char| c.is_whitespace()).parse_next(input)?;
    let rest = std::mem::take(input);
    Ok(rest.to_string())
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
}
