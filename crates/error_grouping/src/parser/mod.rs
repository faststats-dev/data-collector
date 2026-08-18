mod go;
mod java;
mod javascript;
mod php;
mod python;
mod rust;
mod swift;

use crate::ast::{ParserLimits, StackTrace};
use crate::{Language, ParseError};

pub(crate) fn parse<'a>(
    language: Language,
    input: &'a str,
    limits: &ParserLimits,
) -> Result<StackTrace<'a>, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError::InputTooLarge {
            actual: input.len(),
            limit: limits.max_input_bytes,
        });
    }
    if input.trim().is_empty() {
        return Err(ParseError::Empty);
    }

    let lines = ValidatingLines::new(input, limits);
    match language {
        Language::Java => java::parse_lines(lines),
        Language::Rust => rust::parse_lines(lines),
        Language::JavaScript => javascript::parse_lines(lines),
        Language::Python => python::parse_lines(lines),
        Language::Php => php::parse_lines(lines),
        Language::Go => go::parse_lines(lines),
        Language::Swift => swift::parse_lines(lines),
    }
}

struct ValidatingLines<'input, 'limits> {
    lines: std::iter::Enumerate<std::str::Lines<'input>>,
    limits: &'limits ParserLimits,
    failed: bool,
}

impl<'input, 'limits> ValidatingLines<'input, 'limits> {
    fn new(input: &'input str, limits: &'limits ParserLimits) -> Self {
        Self {
            lines: input.lines().enumerate(),
            limits,
            failed: false,
        }
    }
}

impl<'input> Iterator for ValidatingLines<'input, '_> {
    type Item = Result<&'input str, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }

        let (index, line) = self.lines.next()?;
        let line_number = index + 1;
        if line_number > self.limits.max_lines {
            self.failed = true;
            return Some(Err(ParseError::TooManyLines {
                limit: self.limits.max_lines,
            }));
        }
        if line.len() > self.limits.max_line_bytes {
            self.failed = true;
            return Some(Err(ParseError::LineTooLong {
                line: line_number,
                actual: line.len(),
                limit: self.limits.max_line_bytes,
            }));
        }
        Some(Ok(line))
    }
}

pub(crate) fn trim_line(line: &str) -> (&str, usize) {
    let trimmed_start = line.trim_start();
    (trimmed_start.trim_end(), line.len() - trimmed_start.len())
}

pub(crate) fn looks_like_exception(line: &str, extra_kind_chars: &[char]) -> bool {
    let kind = line.split_once(':').map_or(line, |(kind, _)| kind);
    !kind.is_empty()
        && kind.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, '.' | '_')
                || extra_kind_chars.contains(&character)
        })
}

/// Parse a required prefix and return the remaining non-empty payload.
pub(crate) fn payload<'a>(line: &'a str, prefix: &'static str) -> Option<&'a str> {
    line.strip_prefix(prefix)
        .filter(|payload| !payload.is_empty())
}

pub(crate) fn source_file(text: &str) -> &str {
    let text = text.trim();
    let (before_last_number, _) = take_numeric_suffix(text);
    let (before_line, line) = take_numeric_suffix(before_last_number);
    line.map_or(before_last_number, |_| before_line)
}

fn take_numeric_suffix(text: &str) -> (&str, Option<u32>) {
    let Some((head, tail)) = text.rsplit_once(':') else {
        return (text, None);
    };
    tail.parse::<u32>()
        .map_or((text, None), |value| (head, Some(value)))
}

pub(crate) fn error_kind(text: &str) -> Option<&str> {
    let text = text.trim();
    some(text.split_once(':').map_or(text, |(kind, _)| kind).trim())
}

pub(crate) fn some(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_locations_without_confusing_url_or_windows_colons() {
        assert_eq!(
            source_file("https://host:8080/app.js:12:7"),
            "https://host:8080/app.js"
        );
        assert_eq!(source_file(r"C:\work\app.js:9:2"), r"C:\work\app.js");
        assert_eq!(source_file("Main.java:42"), "Main.java");
    }

    #[test]
    fn parsing_reports_the_first_limit_violation() {
        let limits = ParserLimits {
            max_input_bytes: 8,
            max_lines: 1,
            max_line_bytes: 3,
        };
        assert!(matches!(
            parse(Language::Java, "         ", &limits),
            Err(ParseError::InputTooLarge { .. })
        ));
        assert!(matches!(
            parse(Language::Java, "abcd", &limits),
            Err(ParseError::LineTooLong { line: 1, .. })
        ));
        assert!(matches!(
            parse(Language::Java, "a\nb", &limits),
            Err(ParseError::TooManyLines { .. })
        ));
        assert_eq!(
            parse(Language::Java, " \t", &limits),
            Err(ParseError::Empty)
        );
    }

    #[test]
    fn parsing_applies_limits_before_language_recognition() {
        let limits = ParserLimits {
            max_input_bytes: 1_024,
            max_lines: 2,
            max_line_bytes: 1_024,
        };
        assert_eq!(
            parse(
                Language::Java,
                "java.lang.Error: bad\n at app.Main.run(Main.java:1)\ntrailing",
                &limits,
            ),
            Err(ParseError::TooManyLines { limit: 2 })
        );
        assert_eq!(
            parse(Language::Java, " \t", &ParserLimits::default()),
            Err(ParseError::Empty)
        );
    }
}
