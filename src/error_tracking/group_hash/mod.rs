use crate::error_tracking::language::{ErrorLanguage, UNSUPPORTED_LANGUAGE_MESSAGE};
use crate::utils::sha256_hex;
use std::fmt;

pub mod java;
pub mod javascript;
pub mod php;

static JAVA: java::JavaGroupHashProvider = java::JavaGroupHashProvider;
static JAVASCRIPT: javascript::JavascriptGroupHashProvider =
    javascript::JavascriptGroupHashProvider;
static PHP: php::PhpGroupHashProvider = php::PhpGroupHashProvider;

pub trait GroupHashProvider: Sync {
    fn group_hash(&self, error_type: &str, stacktrace: &str) -> String;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GroupHashError {
    language: String,
}

impl GroupHashError {
    pub fn unsupported(language: impl Into<String>) -> Self {
        Self {
            language: language.into(),
        }
    }
}

impl fmt::Display for GroupHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: '{}'", UNSUPPORTED_LANGUAGE_MESSAGE, self.language)
    }
}

impl std::error::Error for GroupHashError {}

pub fn group_hash(
    language: &str,
    error_type: &str,
    stacktrace: &str,
) -> Result<String, GroupHashError> {
    Ok(provider_for_language(language)?.group_hash(error_type, stacktrace))
}

pub fn provider_for_language(
    language: &str,
) -> Result<&'static dyn GroupHashProvider, GroupHashError> {
    match ErrorLanguage::parse(language) {
        Ok(ErrorLanguage::Java) => Ok(&JAVA),
        Ok(ErrorLanguage::Javascript) => Ok(&JAVASCRIPT),
        Ok(ErrorLanguage::Php) => Ok(&PHP),
        Err(error) => Err(GroupHashError::unsupported(error.language())),
    }
}

pub(crate) fn hash_normalized(normalized: &str) -> String {
    sha256_hex(&[normalized.as_bytes()])
}

#[cfg(test)]
mod tests {
    use super::{group_hash, provider_for_language};

    #[test]
    fn dispatches_supported_languages() {
        assert!(provider_for_language(" java ").is_ok());
        assert!(provider_for_language("JavaScript").is_ok());
        assert!(provider_for_language("js").is_ok());
        assert!(provider_for_language("PHP").is_ok());
    }

    #[test]
    fn rejects_unsupported_languages() {
        let error = provider_for_language("ruby")
            .err()
            .expect("ruby should be unsupported");

        assert_eq!(
            error.to_string(),
            "Unsupported language. Expected java, javascript, or php: 'ruby'"
        );
    }

    #[test]
    fn creates_group_hash_through_abstraction() {
        let direct = super::javascript::group_hash("TypeError", " at render (/app/a.js:10:20)");
        let dispatched = group_hash("javascript", "TypeError", " at render (/app/a.js:10:20)")
            .expect("javascript provider");

        assert_eq!(direct, dispatched);
    }
}
