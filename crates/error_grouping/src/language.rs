use std::{error::Error, fmt, str::FromStr};

use crate::ParseError;
use crate::ast::{ParserLimits, StackTrace};

/// Runtime stack-trace syntax supported by the grouping engine.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Language {
    /// Java and JVM stack traces.
    Java,
    /// Rust panic backtraces.
    Rust,
    /// JavaScript stacks in V8 or SpiderMonkey form.
    #[serde(alias = "js")]
    JavaScript,
    /// Python tracebacks, including chained exceptions and exception groups.
    #[serde(alias = "py")]
    Python,
    /// PHP fatal-error stack traces.
    Php,
    /// Go panic and fatal-error traces.
    #[serde(alias = "golang")]
    Go,
    /// Swift runtime and Apple crash-report traces.
    Swift,
}

impl Language {
    /// Return the canonical lowercase storage name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Php => "php",
            Self::Go => "go",
            Self::Swift => "swift",
        }
    }

    #[cfg(test)]
    pub(super) fn parse_stack(self, input: &str) -> Result<StackTrace<'_>, ParseError> {
        self.parse_stack_with_limits(input, &ParserLimits::default())
    }

    pub(super) fn parse_stack_with_limits<'a>(
        self,
        input: &'a str,
        limits: &ParserLimits,
    ) -> Result<StackTrace<'a>, ParseError> {
        crate::parser::parse(self, input, limits)
    }
}

impl FromStr for Language {
    type Err = UnsupportedLanguage;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("java") {
            Ok(Self::Java)
        } else if value.eq_ignore_ascii_case("javascript") || value.eq_ignore_ascii_case("js") {
            Ok(Self::JavaScript)
        } else if value.eq_ignore_ascii_case("python") || value.eq_ignore_ascii_case("py") {
            Ok(Self::Python)
        } else if value.eq_ignore_ascii_case("php") {
            Ok(Self::Php)
        } else if value.eq_ignore_ascii_case("go") || value.eq_ignore_ascii_case("golang") {
            Ok(Self::Go)
        } else if value.eq_ignore_ascii_case("rust") || value.eq_ignore_ascii_case("rs") {
            Ok(Self::Rust)
        } else if value.eq_ignore_ascii_case("swift") {
            Ok(Self::Swift)
        } else {
            Err(UnsupportedLanguage)
        }
    }
}

/// Error returned when a language name is not supported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedLanguage;

impl fmt::Display for UnsupportedLanguage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "Unsupported language. Expected java, javascript, python, php, go, rust, or swift",
        )
    }
}

impl Error for UnsupportedLanguage {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_parse_case_insensitively() {
        for (name, expected) in [
            (" java ", Language::Java),
            ("JavaScript", Language::JavaScript),
            ("js", Language::JavaScript),
            ("py", Language::Python),
            ("PHP", Language::Php),
            ("golang", Language::Go),
            ("rs", Language::Rust),
            ("Swift", Language::Swift),
        ] {
            assert_eq!(name.parse(), Ok(expected));
        }
    }

    #[test]
    fn canonical_names_round_trip_through_serde() {
        for language in [
            Language::Java,
            Language::JavaScript,
            Language::Python,
            Language::Php,
            Language::Go,
            Language::Rust,
            Language::Swift,
        ] {
            let json = serde_json::to_string(&language).unwrap();
            assert_eq!(json, format!("\"{}\"", language.as_str()));
            assert_eq!(serde_json::from_str::<Language>(&json).unwrap(), language);
        }
    }
}
