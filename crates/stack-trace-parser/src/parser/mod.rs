mod go;
mod java;
mod javascript;
mod php;
mod python;
mod rust;
mod swift;

use crate::ast::{ParseWarnings, ParserLimits, StackTrace};
use crate::{Language, ParseError, StackFrame, TraceSegment};

pub(super) const MAX_RETAINED_FRAMES: usize = 256;
const MAX_RETAINED_SEGMENTS: usize = 64;

pub(super) fn push_frame<'a>(
    frames: &mut Vec<StackFrame<'a>>,
    frame: StackFrame<'a>,
    warnings: &mut ParseWarnings,
) {
    if frames.len() < MAX_RETAINED_FRAMES {
        frames.push(frame);
    } else {
        warnings.truncated = true;
    }
}

pub(super) fn push_recent_frame<'a>(
    frames: &mut Vec<StackFrame<'a>>,
    frame: StackFrame<'a>,
    warnings: &mut ParseWarnings,
) {
    if frames.len() == MAX_RETAINED_FRAMES {
        frames.remove(0);
        warnings.truncated = true;
    }
    frames.push(frame);
}

pub(super) fn push_segment<'a>(
    segments: &mut Vec<TraceSegment<'a>>,
    segment: TraceSegment<'a>,
    warnings: &mut ParseWarnings,
) {
    if segments.len() == MAX_RETAINED_SEGMENTS {
        segments.remove(1);
        warnings.truncated = true;
    }
    segments.push(segment);
}

pub(super) fn parse<'a>(
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

    for (index, line) in input.lines().enumerate() {
        if index >= limits.max_lines {
            return Err(ParseError::TooManyLines {
                limit: limits.max_lines,
            });
        }
        if line.len() > limits.max_line_bytes {
            return Err(ParseError::LineTooLong {
                line: index + 1,
                actual: line.len(),
                limit: limits.max_line_bytes,
            });
        }
    }
    let lines = input.lines();
    match language {
        Language::Java => java::parse_lines(lines),
        Language::Rust => rust::parse_lines(lines),
        Language::JavaScript => javascript::parse_lines(lines),
        Language::Python => python::parse_lines(lines),
        Language::Php => php::parse_lines(lines),
        Language::Go => go::parse_lines(lines),
        Language::Swift => swift::parse_lines(lines),
    }
    .ok_or(ParseError::Unrecognized)
}

pub(super) fn trim_line(line: &str) -> (&str, usize) {
    let trimmed_start = line.trim_start();
    (trimmed_start.trim_end(), line.len() - trimmed_start.len())
}

pub(super) fn looks_like_exception(line: &str, extra_kind_chars: &[char]) -> bool {
    let kind = line.split_once(':').map_or(line, |(kind, _)| kind);
    !kind.is_empty()
        && kind.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(character, '.' | '_')
                || extra_kind_chars.contains(&character)
        })
}

pub(super) fn payload<'a>(line: &'a str, prefix: &'static str) -> Option<&'a str> {
    line.strip_prefix(prefix)
        .filter(|payload| !payload.is_empty())
}

pub(super) fn source_file(text: &str) -> &str {
    let text = text.trim();
    let text = strip_numeric_suffix(text).unwrap_or(text);
    strip_numeric_suffix(text).unwrap_or(text)
}

fn strip_numeric_suffix(text: &str) -> Option<&str> {
    let (head, tail) = text.rsplit_once(':')?;
    tail.parse::<u32>().is_ok().then_some(head)
}

pub(super) fn error_kind(text: &str) -> Option<&str> {
    let text = text.trim();
    nonempty(text.split_once(':').map_or(text, |(kind, _)| kind).trim())
}

pub(super) fn nonempty(value: &str) -> Option<&str> {
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
