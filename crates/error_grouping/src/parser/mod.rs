//! Stack trace parsers. The public functions return owned ASTs, so results do
//! not borrow potentially large or short-lived log buffers.

pub mod go;
pub mod java;
pub mod javascript;
pub mod php;
pub mod python;
pub mod rust;

macro_rules! parser_entrypoints {
    () => {
        pub fn parse(input: &str) -> Result<$crate::StackTrace, $crate::ParseError> {
            parse_with_options(input, &$crate::ParserOptions::default())
        }

        pub fn parse_with_options(
            input: &str,
            options: &$crate::ParserOptions,
        ) -> Result<$crate::StackTrace, $crate::ParseError> {
            let mut lines = $crate::parser::CheckedLines::new(input, options)?;
            let result = parse_lines(&mut lines, options);
            lines.finish()?;
            result
        }

        pub(crate) fn parse_validated(
            input: &str,
            options: &$crate::ParserOptions,
        ) -> Result<$crate::StackTrace, $crate::ParseError> {
            parse_lines(input.lines(), options)
        }
    };
}
pub(crate) use parser_entrypoints;

use crate::ast::{
    ErrorInfo, ParseError, ParserOptions, SegmentRelation, SourceLocation, TraceSegment,
};

#[derive(Default)]
pub(crate) struct ExceptionTreeBuilder {
    pub segments: Vec<TraceSegment>,
    scopes: Vec<(usize, usize)>,
}

impl ExceptionTreeBuilder {
    pub fn add(
        &mut self,
        indent: usize,
        relation: SegmentRelation,
        error: &str,
        thread: Option<String>,
    ) {
        let root = self.segments.is_empty() || relation == SegmentRelation::Root;
        if root {
            self.scopes.clear();
        } else {
            while self.scopes.len() > 1
                && self
                    .scopes
                    .last()
                    .is_some_and(|(level, _)| *level >= indent)
            {
                self.scopes.pop();
            }
        }
        let index = self.segments.len();
        let (kind, message) = error_parts(error);
        self.segments.push(TraceSegment {
            parent: (!root).then(|| {
                self.scopes
                    .last()
                    .map(|(_, index)| *index)
                    .expect("a related error must have a parent scope")
            }),
            relation: if root {
                SegmentRelation::Root
            } else {
                relation
            },
            error: ErrorInfo {
                kind,
                message,
                thread,
                location: None,
            },
            ..TraceSegment::default()
        });
        self.scopes.push((indent, index));
    }

    pub fn current(&mut self) -> &mut TraceSegment {
        if self.segments.is_empty() {
            self.segments.push(TraceSegment::default());
            self.scopes.push((0, 0));
        }
        self.segments.last_mut().expect("root segment exists")
    }
}

pub(crate) fn trim_line(line: &str) -> (&str, usize) {
    let trimmed_start = line.trim_start();
    (trimmed_start.trim_end(), line.len() - trimmed_start.len())
}

#[derive(Default)]
pub(crate) struct DetectionHints<'a> {
    header: &'a str,
    java_frame: bool,
    php_frame: bool,
    rust_marker: bool,
    javascript_frame: bool,
    complete: bool,
}

impl DetectionHints<'_> {
    pub(crate) fn looks_like_python(&self) -> bool {
        self.header == "Traceback (most recent call last):"
    }

    pub(crate) fn looks_like_go(&self) -> bool {
        self.header.starts_with("panic:")
            || self.header.starts_with("fatal error:")
            || self.header.starts_with("goroutine ")
    }

    pub(crate) fn looks_like_java(&self) -> bool {
        if self.header.starts_with("Exception in thread ") || self.header.starts_with("Caused by:")
        {
            return true;
        }
        let kind = self
            .header
            .split_once(':')
            .map_or(self.header, |(kind, _)| kind);
        !kind.contains(char::is_whitespace) && self.java_frame
    }

    pub(crate) fn looks_like_php(&self) -> bool {
        self.header.starts_with("PHP Fatal error:")
            || self.header.starts_with("Fatal error:")
            || self.header.starts_with("Uncaught ")
            || self.php_frame
    }

    pub(crate) fn looks_like_rust(&self) -> bool {
        (self.header.starts_with("thread '") && self.header.contains("panicked at"))
            || self.rust_marker
    }

    pub(crate) fn looks_like_javascript(&self) -> bool {
        let kind = self
            .header
            .split_once(':')
            .map_or(self.header, |(kind, _)| kind);
        kind.ends_with("Error") && self.javascript_frame
    }
}

pub(crate) fn validate_and_detect<'a>(
    input: &'a str,
    options: &ParserOptions,
) -> Result<DetectionHints<'a>, ParseError> {
    let mut lines = CheckedLines::new(input, options)?;
    let mut hints = DetectionHints::default();
    let mut is_header = true;

    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !hints.complete {
            hints.inspect(trimmed, is_header);
        }
        is_header = false;
    }
    lines.finish()?;
    Ok(hints)
}

impl<'a> DetectionHints<'a> {
    fn inspect(&mut self, trimmed: &'a str, is_header: bool) {
        if is_header {
            self.header = trimmed;
            self.complete = self.looks_like_python()
                || self.looks_like_go()
                || self.header.starts_with("Exception in thread ")
                || self.header.starts_with("Caused by:")
                || self.header.starts_with("PHP Fatal error:")
                || self.header.starts_with("Fatal error:")
                || self.header.starts_with("Uncaught ")
                || (self.header.starts_with("thread '") && self.header.contains("panicked at"));
            if self.complete {
                return;
            }
        }

        self.rust_marker |= trimmed == "stack backtrace:";
        if !is_header {
            match trimmed.as_bytes().first() {
                Some(b'#') => self.php_frame |= trimmed.starts_with("#0 "),
                Some(b'a') => {
                    self.javascript_frame |= trimmed.starts_with("at ") || trimmed.contains('@');
                    self.java_frame |= looks_like_java_frame(trimmed);
                }
                _ => self.javascript_frame |= trimmed.contains('@'),
            }
        }
        if self.java_frame || self.php_frame || self.javascript_frame {
            self.complete =
                self.looks_like_java() || self.looks_like_php() || self.looks_like_javascript();
        }
    }
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
                actual: line_number,
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

fn looks_like_java_frame(line: &str) -> bool {
    let Some((callable, source)) = line
        .strip_prefix("at ")
        .and_then(|body| body.rsplit_once('('))
    else {
        return false;
    };
    callable.contains('.')
        && source.ends_with(')')
        && (source.starts_with("Native Method")
            || source.starts_with("Unknown Source")
            || source.contains(".java:"))
}

pub(crate) struct UnparsedLines<'a> {
    lines: Option<Vec<&'a str>>,
}

impl<'a> UnparsedLines<'a> {
    pub(crate) fn new(options: &ParserOptions) -> Self {
        Self {
            lines: options.retain_unparsed_lines.then(Vec::new),
        }
    }

    pub(crate) fn push(&mut self, line: &'a str) {
        if let Some(lines) = &mut self.lines {
            lines.push(line);
        }
    }

    pub(crate) fn finish_trace(
        self,
        details: crate::TraceDetails,
        segments: Vec<TraceSegment>,
    ) -> crate::StackTrace {
        let lines = self
            .lines
            .unwrap_or_default()
            .into_iter()
            .map(str::to_owned)
            .collect();
        crate::StackTrace::new(details, segments, lines)
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

pub(crate) fn split_location(text: &str) -> SourceLocation {
    let text = text.trim();
    let (before_column, column) = take_numeric_suffix(text);
    let (file, line) = take_numeric_suffix(before_column);
    if line.is_some() {
        SourceLocation {
            file: file.to_owned(),
            line,
            column,
        }
    } else {
        SourceLocation {
            file: before_column.to_owned(),
            line: column,
            column: None,
        }
    }
}

fn take_numeric_suffix(text: &str) -> (&str, Option<u32>) {
    let Some((head, tail)) = text.rsplit_once(':') else {
        return (text, None);
    };
    tail.parse::<u32>()
        .map_or((text, None), |value| (head, Some(value)))
}

pub(crate) fn error_parts(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim();
    match text.split_once(':') {
        Some((kind, message)) => (some(kind.trim()), some(message.trim_start())),
        None => (some(text), None),
    }
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
            split_location("https://host:8080/app.js:12:7"),
            SourceLocation {
                file: "https://host:8080/app.js".to_owned(),
                line: Some(12),
                column: Some(7),
            }
        );
        assert_eq!(
            split_location(r"C:\work\app.js:9:2").file,
            r"C:\work\app.js"
        );
        assert_eq!(
            split_location("Main.java:42"),
            SourceLocation {
                file: "Main.java".to_owned(),
                line: Some(42),
                column: None,
            }
        );
    }

    #[test]
    fn checked_lines_validate_all_limits_without_allocating() {
        let options = ParserOptions {
            max_input_bytes: 8,
            max_lines: 1,
            max_line_bytes: 3,
            retain_unparsed_lines: false,
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
            retain_unparsed_lines: false,
        };
        assert_eq!(
            java::parse_with_options(
                "java.lang.Error: bad\n at app.Main.run(Main.java:1)\ntrailing",
                &options,
            ),
            Err(ParseError::TooManyLines {
                actual: 3,
                limit: 2,
            })
        );
        assert_eq!(java::parse(" \t"), Err(ParseError::Empty));
    }

    #[test]
    fn diagnostics_default_to_off() {
        assert!(!ParserOptions::default().retain_unparsed_lines);
    }
}
