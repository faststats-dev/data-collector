pub const UNSUPPORTED_LANGUAGE_MESSAGE: &str =
    "Unsupported language. Expected java, javascript, python, php, go, or rust";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum ErrorLanguage {
    #[default]
    Java,
    Javascript,
    Python,
    Php,
    Go,
    Rust,
}

impl ErrorLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Javascript => "javascript",
            Self::Python => "python",
            Self::Php => "php",
            Self::Go => "go",
            Self::Rust => "rust",
        }
    }

    pub fn parse(value: &str) -> Result<Self, UnsupportedLanguage> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("java") {
            Ok(Self::Java)
        } else if value.eq_ignore_ascii_case("javascript") || value.eq_ignore_ascii_case("js") {
            Ok(Self::Javascript)
        } else if value.eq_ignore_ascii_case("python") || value.eq_ignore_ascii_case("py") {
            Ok(Self::Python)
        } else if value.eq_ignore_ascii_case("php") {
            Ok(Self::Php)
        } else if value.eq_ignore_ascii_case("go") || value.eq_ignore_ascii_case("golang") {
            Ok(Self::Go)
        } else if value.eq_ignore_ascii_case("rust") || value.eq_ignore_ascii_case("rs") {
            Ok(Self::Rust)
        } else {
            Err(UnsupportedLanguage(value.to_ascii_lowercase()))
        }
    }

    pub fn parse_optional(value: Option<&str>) -> Result<Self, &'static str> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => Self::parse(value).map_err(|_| UNSUPPORTED_LANGUAGE_MESSAGE),
            None => Ok(Self::default()),
        }
    }

    pub fn group_hash(self, error_type: &str, stacktrace: &str) -> String {
        let language = self.into();
        match error_grouping::parse_language(language, stacktrace) {
            Ok(trace) => {
                error_grouping::fingerprint_with_kind(&trace, Some(error_type)).to_string()
            }
            Err(_) => {
                metrics::counter!(
                    "error_grouping_parse_failures_total",
                    "language" => self.as_str()
                )
                .increment(1);
                error_grouping::fingerprint_error(language, error_type).to_string()
            }
        }
    }
}

impl From<ErrorLanguage> for error_grouping::Language {
    fn from(language: ErrorLanguage) -> Self {
        match language {
            ErrorLanguage::Java => Self::Java,
            ErrorLanguage::Javascript => Self::JavaScript,
            ErrorLanguage::Python => Self::Python,
            ErrorLanguage::Php => Self::Php,
            ErrorLanguage::Go => Self::Go,
            ErrorLanguage::Rust => Self::Rust,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnsupportedLanguage(String);

#[cfg(test)]
mod tests {
    use super::{ErrorLanguage, UNSUPPORTED_LANGUAGE_MESSAGE};

    #[test]
    fn parses_supported_languages_and_aliases() {
        assert_eq!(ErrorLanguage::parse(" java ").unwrap(), ErrorLanguage::Java);
        assert_eq!(
            ErrorLanguage::parse("JavaScript").unwrap(),
            ErrorLanguage::Javascript
        );
        assert_eq!(
            ErrorLanguage::parse("js").unwrap(),
            ErrorLanguage::Javascript
        );
        assert_eq!(ErrorLanguage::parse("py").unwrap(), ErrorLanguage::Python);
        assert_eq!(ErrorLanguage::parse("PHP").unwrap(), ErrorLanguage::Php);
        assert_eq!(ErrorLanguage::parse("golang").unwrap(), ErrorLanguage::Go);
        assert_eq!(ErrorLanguage::parse("Rust").unwrap(), ErrorLanguage::Rust);
        assert_eq!(ErrorLanguage::parse("rs").unwrap(), ErrorLanguage::Rust);
    }

    #[test]
    fn serializes_to_canonical_storage_values() {
        assert_eq!(ErrorLanguage::Java.as_str(), "java");
        assert_eq!(ErrorLanguage::Javascript.as_str(), "javascript");
        assert_eq!(ErrorLanguage::Python.as_str(), "python");
        assert_eq!(ErrorLanguage::Php.as_str(), "php");
        assert_eq!(ErrorLanguage::Go.as_str(), "go");
        assert_eq!(ErrorLanguage::Rust.as_str(), "rust");
    }

    #[test]
    fn parse_optional_defaults_to_java() {
        assert_eq!(
            ErrorLanguage::parse_optional(None).unwrap(),
            ErrorLanguage::Java
        );
        assert_eq!(
            ErrorLanguage::parse_optional(Some("")).unwrap(),
            ErrorLanguage::Java
        );
    }

    #[test]
    fn rejects_unsupported_languages() {
        let error = ErrorLanguage::parse(" RUBY ").unwrap_err();

        assert_eq!(error.0, "ruby");
        assert_eq!(
            ErrorLanguage::parse_optional(Some("ruby")).unwrap_err(),
            UNSUPPORTED_LANGUAGE_MESSAGE
        );
    }

    #[test]
    fn grouping_uses_the_authoritative_type_and_versioned_new_algorithm() {
        let stack = "at render (/app/main.js:10:2)";
        let type_error = ErrorLanguage::Javascript.group_hash("TypeError", stack);
        let range_error = ErrorLanguage::Javascript.group_hash("RangeError", stack);

        assert!(type_error.starts_with("eg1_"));
        assert_ne!(type_error, range_error);
    }

    #[test]
    fn header_and_frame_only_payloads_group_together() {
        let frames =
            ErrorLanguage::Javascript.group_hash("TypeError", "at render (/app/main.js:10:2)");
        let full = ErrorLanguage::Javascript.group_hash(
            "TypeError",
            "TypeError: dynamic message\nat render (/app/main.js:99:8)",
        );

        assert_eq!(frames, full);
    }

    #[test]
    fn supports_python_and_go_grouping() {
        let python = ErrorLanguage::Python.group_hash(
            "ValueError",
            "Traceback (most recent call last):\n  File \"/app/main.py\", line 1, in run\nValueError: x",
        );
        let go = ErrorLanguage::Go.group_hash(
            "panic",
            "panic: bad\n\ngoroutine 1 [running]:\nmain.run()\n\t/app/main.go:3 +0x1",
        );

        assert!(python.starts_with("eg1_"));
        assert!(go.starts_with("eg1_"));
        assert_ne!(python, go);
    }

    #[test]
    fn unparsed_stacks_use_only_new_type_identity() {
        let first = ErrorLanguage::Java.group_hash("FirstError", "not a stack");
        let second = ErrorLanguage::Java.group_hash("SecondError", "not a stack");
        let noisy = ErrorLanguage::Java.group_hash("FirstError", "different unsupported text");

        assert_eq!(first, noisy);
        assert_ne!(first, second);
    }
}
