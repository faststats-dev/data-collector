use crate::error_tracking::group_hash;

pub const UNSUPPORTED_LANGUAGE_MESSAGE: &str =
    "Unsupported language. Expected java, javascript, or php";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum ErrorLanguage {
    #[default]
    Java,
    Javascript,
    Php,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MappingKind {
    Proguard,
    SourceMap,
}

impl MappingKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Proguard => "r8",
            Self::SourceMap => "javascript",
        }
    }
}

impl ErrorLanguage {
    pub fn parse(value: &str) -> Result<Self, UnsupportedLanguage> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("java") {
            Ok(Self::Java)
        } else if value.eq_ignore_ascii_case("javascript") || value.eq_ignore_ascii_case("js") {
            Ok(Self::Javascript)
        } else if value.eq_ignore_ascii_case("php") {
            Ok(Self::Php)
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
        }
    }

    pub(crate) const fn mapping_kind(self) -> Option<MappingKind> {
        match self {
            Self::Java => Some(MappingKind::Proguard),
            Self::Javascript => Some(MappingKind::SourceMap),
            Self::Php => None,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnsupportedLanguage(String);

#[cfg(test)]
mod tests {
    use super::{ErrorLanguage, MappingKind, UNSUPPORTED_LANGUAGE_MESSAGE};

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
    fn declares_language_capabilities_in_one_place() {
        assert_eq!(
            [
                ErrorLanguage::Java.mapping_kind(),
                ErrorLanguage::Javascript.mapping_kind(),
                ErrorLanguage::Php.mapping_kind(),
            ],
            [
                Some(MappingKind::Proguard),
                Some(MappingKind::SourceMap),
                None,
            ]
        );
    }
}
