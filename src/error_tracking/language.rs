pub const UNSUPPORTED_LANGUAGE_MESSAGE: &str =
    "Unsupported language. Expected java, javascript, or php";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum ErrorLanguage {
    #[default]
    Java,
    Javascript,
    Php,
}

impl ErrorLanguage {
    pub fn parse(value: &str) -> Result<Self, UnsupportedLanguage> {
        match normalize_language(value).as_deref() {
            Some("java") => Ok(Self::Java),
            Some("javascript" | "js") => Ok(Self::Javascript),
            Some("php") => Ok(Self::Php),
            Some(normalized) => Err(UnsupportedLanguage::new(normalized)),
            None => Err(UnsupportedLanguage::new(value)),
        }
    }

    pub fn parse_optional(value: Option<&str>) -> Result<Self, &'static str> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            Some(value) => Self::parse(value).map_err(|_| UNSUPPORTED_LANGUAGE_MESSAGE),
            None => Ok(Self::default()),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnsupportedLanguage {
    language: String,
}

impl UnsupportedLanguage {
    fn new(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
        }
    }
}

fn normalize_language(language: &str) -> Option<String> {
    let language = language.trim();
    (!language.is_empty()).then(|| language.to_ascii_lowercase())
}

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
        let error = ErrorLanguage::parse("ruby").unwrap_err();

        assert_eq!(error.language, "ruby");
        assert_eq!(
            ErrorLanguage::parse_optional(Some("ruby")).unwrap_err(),
            UNSUPPORTED_LANGUAGE_MESSAGE
        );
    }
}
