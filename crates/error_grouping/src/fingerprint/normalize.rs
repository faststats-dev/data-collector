use std::borrow::Cow;

use crate::Language;
use crate::ast::StackFrame;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct FrameIdentity<'a> {
    pub(super) function: Option<Cow<'a, str>>,
    pub(super) module: Option<Cow<'a, str>>,
    pub(super) file: Option<Cow<'a, str>>,
}

pub(super) fn frame_identity<'a>(language: Language, frame: &StackFrame<'a>) -> FrameIdentity<'a> {
    let function = frame
        .function
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_function(language, value));
    let module = frame
        .module
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_case(language, value));
    let file = frame.file.and_then(|file| normalized_file(language, file));
    FrameIdentity {
        function,
        module,
        file,
    }
}

pub(super) fn normalized_kind(language: Language, kind: Option<&str>) -> Option<Cow<'_, str>> {
    kind.map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| normalize_case(language, kind))
}

fn normalize_function(language: Language, function: &str) -> Cow<'_, str> {
    if language == Language::Rust
        && let Some((prefix, suffix)) = function.rsplit_once("::h")
        && suffix.len() >= 16
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Cow::Borrowed(prefix);
    }
    if language == Language::Java {
        return normalize_java_generated_function(function);
    }
    normalize_case(language, function)
}

fn normalized_file(language: Language, file: &str) -> Option<Cow<'_, str>> {
    let file = file.trim().split(['?', '#']).next().unwrap_or("");
    let file = file.trim_end_matches(['/', '\\']);
    if file.is_empty() {
        return None;
    }

    if file.contains('\\') {
        let normalized = file.replace('\\', "/");
        Some(Cow::Owned(
            normalize_file_path(language, &normalized).into_owned(),
        ))
    } else {
        Some(normalize_file_path(language, file))
    }
}

fn normalize_file_path(language: Language, path: &str) -> Cow<'_, str> {
    let path = path.trim_start_matches('/');
    let mut offset = 0;
    let mut stable_root = None;
    for component in path.split('/') {
        if is_stable_path_root(component) {
            stable_root = Some(offset);
        }
        offset += component.len() + 1;
    }

    let suffix = stable_root.map_or_else(
        || path.rsplit_once('/').map_or(path, |(_, basename)| basename),
        |root| &path[root..],
    );
    normalize_asset_hashes(normalize_case(language, suffix))
}

fn is_stable_path_root(component: &str) -> bool {
    matches!(
        component,
        "src" | "tests" | "test" | "app" | "lib" | "crates" | "packages"
    )
}

fn normalize_case(language: Language, value: &str) -> Cow<'_, str> {
    if language == Language::Php && value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(value.to_ascii_lowercase())
    } else {
        Cow::Borrowed(value)
    }
}

fn normalize_java_generated_function(function: &str) -> Cow<'_, str> {
    let bytes = function.as_bytes();
    let generated = function.contains("$$Lambda$")
        || function.contains("$Proxy")
        || bytes
            .windows(2)
            .any(|pair| pair[0] == b'$' && pair[1].is_ascii_digit());
    if !generated {
        return Cow::Borrowed(function);
    }

    let mut output = String::with_capacity(function.len());
    let mut index = 0;
    let mut changed = false;
    while index < bytes.len() {
        if function[index..].starts_with("$$Lambda$") {
            output.push_str("$$Lambda");
            index += "$$Lambda$".len();
            while index < bytes.len()
                && (bytes[index].is_ascii_hexdigit() || matches!(bytes[index], b'x' | b'X' | b'/'))
            {
                index += 1;
            }
            changed = true;
        } else if function[index..].starts_with("$Proxy") {
            output.push_str("$Proxy");
            index += "$Proxy".len();
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
                changed = true;
            }
        } else if bytes[index] == b'$' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            output.push_str("$anon");
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            changed = true;
        } else {
            let Some(character) = function[index..].chars().next() else {
                break;
            };
            output.push(character);
            index += character.len_utf8();
        }
    }

    if changed {
        Cow::Owned(output)
    } else {
        Cow::Borrowed(function)
    }
}

fn normalize_asset_hashes(value: Cow<'_, str>) -> Cow<'_, str> {
    if !value.split(['.', '-']).any(is_asset_hash) {
        return value;
    }

    let mut output = String::with_capacity(value.len());
    for component in value.split_inclusive(['.', '-']) {
        let token = component.trim_end_matches(['.', '-']);
        output.push_str(if is_asset_hash(token) {
            "<hash>"
        } else {
            token
        });
        output.push_str(&component[token.len()..]);
    }
    Cow::Owned(output)
}

fn is_asset_hash(value: &str) -> bool {
    value.len() >= 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn is_runtime_frame(language: Language, frame: &StackFrame<'_>) -> bool {
    let function = frame.function.unwrap_or("");
    let file = frame.file.unwrap_or("");
    match language {
        Language::Java => ["java.", "javax.", "jdk.", "sun.", "com.sun."]
            .iter()
            .any(|prefix| function.starts_with(prefix)),
        Language::Rust => {
            [
                "std::",
                "core::",
                "alloc::",
                "backtrace::",
                "rustc_demangle::",
                "<std::",
                "<core::",
                "<alloc::",
            ]
            .iter()
            .any(|prefix| function.starts_with(prefix))
                || function == "rust_begin_unwind"
                || function.starts_with("__rust")
                || function.starts_with("_rust")
                || function.contains(" as core::ops::function::")
        }
        Language::JavaScript => {
            file.starts_with("node:internal/")
                || file.starts_with("internal/")
                || function.starts_with("node:internal/")
        }
        Language::Python => file.starts_with("<frozen "),
        Language::Php => false,
        Language::Go => ["runtime.", "runtime/", "testing."]
            .iter()
            .any(|prefix| function.starts_with(prefix)),
        Language::Swift => {
            frame
                .module
                .is_some_and(|module| module.starts_with("libswift"))
                || function.starts_with("Swift.")
                || function.starts_with("swift_")
                || function.starts_with("_assertionFailure")
        }
    }
}
