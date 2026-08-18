//! Stack trace parsers. The public functions return owned ASTs, so results do
//! not borrow potentially large or short-lived log buffers.

mod go;
mod java;
mod javascript;
mod php;
mod python;
mod rust;
mod swift;

use crate::{Language, ParseError, ParserOptions, StackTrace};

pub(crate) fn parse(
    language: Language,
    input: &str,
    options: &ParserOptions,
) -> Result<StackTrace, ParseError> {
    let mut lines = CheckedLines::new(input, options)?;
    let result = match language {
        Language::Java => java::parse_lines(&mut lines),
        Language::Rust => rust::parse_lines(&mut lines),
        Language::JavaScript => javascript::parse_lines(&mut lines),
        Language::Python => python::parse_lines(&mut lines),
        Language::Php => php::parse_lines(&mut lines),
        Language::Go => go::parse_lines(&mut lines),
        Language::Swift => swift::parse_lines(&mut lines),
    };
    lines.finish()?;
    result
}

pub(crate) fn trim_line(line: &str) -> (&str, usize) {
    let trimmed_start = line.trim_start();
    (trimmed_start.trim_end(), line.len() - trimmed_start.len())
}

pub(crate) struct CheckedLines<'input, 'options> {
    lines: std::iter::Enumerate<std::str::Lines<'input>>,
    options: &'options ParserOptions,
    saw_content: bool,
    error: Option<ParseError>,
}

impl<'input, 'options> CheckedLines<'input, 'options> {
    pub(crate) fn new(
        input: &'input str,
        options: &'options ParserOptions,
    ) -> Result<Self, ParseError> {
        if input.len() > options.max_input_bytes {
            return Err(ParseError::InputTooLarge {
                actual: input.len(),
                limit: options.max_input_bytes,
            });
        }
        Ok(Self {
            lines: input.lines().enumerate(),
            options,
            saw_content: false,
            error: None,
        })
    }

    pub(crate) fn finish(mut self) -> Result<(), ParseError> {
        while self.next().is_some() {}
        if let Some(error) = self.error {
            Err(error)
        } else if self.saw_content {
            Ok(())
        } else {
            Err(ParseError::Empty)
        }
    }
}

impl<'input> Iterator for CheckedLines<'input, '_> {
    type Item = &'input str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.error.is_some() {
            return None;
        }
        let (index, line) = self.lines.next()?;
        let line_number = index + 1;
        if line_number > self.options.max_lines {
            self.error = Some(ParseError::TooManyLines {
                limit: self.options.max_lines,
            });
            return None;
        }
        if line.len() > self.options.max_line_bytes {
            self.error = Some(ParseError::LineTooLong {
                line: line_number,
                actual: line.len(),
                limit: self.options.max_line_bytes,
            });
            return None;
        }
        if !self.saw_content {
            self.saw_content = !line.trim().is_empty();
        }
        Some(line)
    }
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

pub(crate) fn source_file(text: &str) -> String {
    let text = text.trim();
    let (before_last_number, _) = take_numeric_suffix(text);
    let (before_line, line) = take_numeric_suffix(before_last_number);
    line.map_or(before_last_number, |_| before_line).to_owned()
}

fn take_numeric_suffix(text: &str) -> (&str, Option<u32>) {
    let Some((head, tail)) = text.rsplit_once(':') else {
        return (text, None);
    };
    tail.parse::<u32>()
        .map_or((text, None), |value| (head, Some(value)))
}

pub(crate) fn error_kind(text: &str) -> Option<String> {
    let text = text.trim();
    some(text.split_once(':').map_or(text, |(kind, _)| kind).trim())
}

pub(crate) fn some(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
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
    fn checked_lines_validate_all_limits_without_allocating() {
        let options = ParserOptions {
            max_input_bytes: 8,
            max_lines: 1,
            max_line_bytes: 3,
        };
        assert!(matches!(
            CheckedLines::new("         ", &options).and_then(CheckedLines::finish),
            Err(ParseError::InputTooLarge { .. })
        ));
        assert!(matches!(
            CheckedLines::new("abcd", &options).and_then(CheckedLines::finish),
            Err(ParseError::LineTooLong { line: 1, .. })
        ));
        assert!(matches!(
            CheckedLines::new("a\nb", &options).and_then(CheckedLines::finish),
            Err(ParseError::TooManyLines { .. })
        ));
        assert_eq!(
            CheckedLines::new(" \t", &options).and_then(CheckedLines::finish),
            Err(ParseError::Empty)
        );
    }

    #[test]
    fn direct_parsers_apply_checks_during_parsing() {
        let options = ParserOptions {
            max_input_bytes: 1_024,
            max_lines: 2,
            max_line_bytes: 1_024,
        };
        assert_eq!(
            parse(
                Language::Java,
                "java.lang.Error: bad\n at app.Main.run(Main.java:1)\ntrailing",
                &options,
            ),
            Err(ParseError::TooManyLines { limit: 2 })
        );
        assert_eq!(Language::Java.parse_stack(" \t"), Err(ParseError::Empty));
    }
}
