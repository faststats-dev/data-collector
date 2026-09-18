use std::{error::Error, fmt, str::FromStr};

use crate::ParseError;
use crate::ast::{ParserLimits, StackTrace};

/// Runtime stack-trace syntax supported by the parser.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Java,
    Rust,
    JavaScript,
    Python,
    Php,
    Go,
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

    /// Parse with default resource limits, borrowing names from the input.
    pub fn parse_stack(self, input: &str) -> Result<StackTrace<'_>, ParseError> {
        self.parse_stack_with_limits(input, &ParserLimits::default())
    }

    /// Parse with caller-supplied limits. Malformed frames are reported as warnings.
    pub fn parse_stack_with_limits<'a>(
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
        match value.trim().to_ascii_lowercase().as_str() {
            "java" | "jvm" | "kotlin" | "scala" => Ok(Self::Java),
            "javascript" | "js" | "typescript" | "ts" => Ok(Self::JavaScript),
            "python" | "py" => Ok(Self::Python),
            "php" => Ok(Self::Php),
            "go" | "golang" => Ok(Self::Go),
            "rust" | "rs" => Ok(Self::Rust),
            "swift" => Ok(Self::Swift),
            _ => Err(UnsupportedLanguage),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Language {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
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

    #[test]
    fn serde_uses_the_same_trimmed_case_insensitive_aliases_as_from_str() {
        for (json, expected) in [
            (r#"" JS ""#, Language::JavaScript),
            (r#""Py""#, Language::Python),
            (r#""golang""#, Language::Go),
            (r#""RS""#, Language::Rust),
        ] {
            assert_eq!(serde_json::from_str::<Language>(json).unwrap(), expected);
        }
    }
}
