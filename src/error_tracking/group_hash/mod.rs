use crate::error_tracking::language::ErrorLanguage;
use crate::utils::sha256_hex;

pub mod java;
pub mod javascript;
pub mod php;

pub fn group_hash(language: ErrorLanguage, error_type: &str, stacktrace: &str) -> String {
    match language {
        ErrorLanguage::Java => java::group_hash(error_type, stacktrace),
        ErrorLanguage::Javascript => javascript::group_hash(error_type, stacktrace),
        ErrorLanguage::Php => php::group_hash(error_type, stacktrace),
    }
}

pub(crate) fn hash_normalized(normalized: &str) -> String {
    sha256_hex(&[normalized.as_bytes()])
}

#[cfg(test)]
mod tests {
    use super::{ErrorLanguage, group_hash};

    #[test]
    fn dispatches_supported_languages() {
        assert_eq!(
            group_hash(
                ErrorLanguage::Java,
                "Error",
                "at com.test.App.run(App.java:1)"
            ),
            super::java::group_hash("Error", "at com.test.App.run(App.java:1)")
        );
        assert_eq!(
            group_hash(
                ErrorLanguage::Javascript,
                "TypeError",
                " at render (/app/a.js:10:20)"
            ),
            super::javascript::group_hash("TypeError", " at render (/app/a.js:10:20)")
        );
        assert_eq!(
            group_hash(
                ErrorLanguage::Php,
                "RuntimeException",
                "#0 /app/a.php(1): run()"
            ),
            super::php::group_hash("RuntimeException", "#0 /app/a.php(1): run()")
        );
    }

    #[test]
    fn creates_group_hash_through_abstraction() {
        let direct = super::javascript::group_hash("TypeError", " at render (/app/a.js:10:20)");
        let dispatched = group_hash(
            ErrorLanguage::Javascript,
            "TypeError",
            " at render (/app/a.js:10:20)",
        );

        assert_eq!(direct, dispatched);
    }
}
