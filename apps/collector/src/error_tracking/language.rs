use crate::error_tracking::group_hash;

pub const UNSUPPORTED_LANGUAGE_MESSAGE: &str =
    "Unsupported language. Expected java, javascript, php, or rust";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum ErrorLanguage {
    #[default]
    Java,
    Javascript,
    Php,
    Rust,
}

impl ErrorLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Javascript => "javascript",
            Self::Php => "php",
            Self::Rust => "rust",
        }
    }

    pub fn parse(value: &str) -> Result<Self, UnsupportedLanguage> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("java") {
            Ok(Self::Java)
        } else if value.eq_ignore_ascii_case("javascript") || value.eq_ignore_ascii_case("js") {
            Ok(Self::Javascript)
        } else if value.eq_ignore_ascii_case("php") {
            Ok(Self::Php)
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
        match self {
            Self::Java => group_hash::java::group_hash(error_type, stacktrace),
            Self::Javascript => group_hash::javascript::group_hash(error_type, stacktrace),
            Self::Php => group_hash::php::group_hash(error_type, stacktrace),
            Self::Rust => group_hash::rust::group_hash(error_type, stacktrace),
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
        assert_eq!(ErrorLanguage::parse("PHP").unwrap(), ErrorLanguage::Php);
        assert_eq!(ErrorLanguage::parse("Rust").unwrap(), ErrorLanguage::Rust);
        assert_eq!(ErrorLanguage::parse("rs").unwrap(), ErrorLanguage::Rust);
    }

    #[test]
    fn serializes_to_canonical_storage_values() {
        assert_eq!(ErrorLanguage::Java.as_str(), "java");
        assert_eq!(ErrorLanguage::Javascript.as_str(), "javascript");
        assert_eq!(ErrorLanguage::Php.as_str(), "php");
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
}
