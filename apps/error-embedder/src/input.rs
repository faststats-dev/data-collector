use stack_trace_parser::{Language, SegmentRelation, StackTrace};
use serde::Deserialize;

use crate::normalize;

#[derive(Deserialize)]
pub struct Input {
    #[serde(default)]
    pub language: String,
    pub error_type: String,
    pub error_message: String,
    pub stacktrace: String,
    pub mapped_stacktrace: Option<String>,
}

// Bound text preparation independently of the tokenizer and parser input limit.
const MAX_FIELD_BYTES: usize = 4096;
const MAX_FRAMES: usize = 16;

pub(crate) fn bounded(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

// Choose the terminal primary cause, never a suppressed sibling (or its causes).
fn primary_segment(trace: &StackTrace<'_>) -> usize {
    let mut selected = 0;
    let mut suppressed = None;
    for (index, segment) in trace.segments.iter().enumerate() {
        if let Some(depth) = suppressed {
            if segment.depth > depth
                || (segment.depth == depth && segment.relation == SegmentRelation::Cause)
            {
                continue;
            }
            suppressed = None;
        }
        if segment.relation == SegmentRelation::Context {
            break;
        }
        if segment.relation == SegmentRelation::Suppressed {
            suppressed = Some(segment.depth);
        } else if segment.relation == SegmentRelation::Cause {
            selected = index;
        }
    }
    selected
}

impl Input {
    pub fn text(&self) -> String {
        let stack = self
            .mapped_stacktrace
            .as_deref()
            .filter(|stack| !stack.trim().is_empty())
            .unwrap_or(&self.stacktrace);
        // Older collector records omit the language; ingestion defaults to Java.
        let language = if self.language.trim().is_empty() {
            "java"
        } else {
            &self.language
        };
        let trace = language
            .parse::<Language>()
            .ok()
            .and_then(|language| language.parse_stack(stack).ok())
            .filter(|trace| !trace.warnings.malformed_frame && !trace.warnings.truncated);
        let selected = trace.as_ref().and_then(|trace| {
            let index = primary_segment(trace);
            trace.segments.get(index).map(|segment| (index, segment))
        });
        let (kind, message) = match selected {
            Some((index, segment)) if index > 0 => (
                segment.error_kind.unwrap_or(&self.error_type),
                segment.error_message.unwrap_or(""),
            ),
            Some((_, segment)) if self.error_message.trim().is_empty() => (
                self.error_type.as_str(),
                segment.error_message.unwrap_or(""),
            ),
            _ => (self.error_type.as_str(), self.error_message.as_str()),
        };
        let mut output = String::with_capacity(1024);
        output.push_str(bounded(kind, 256).trim());
        output.push_str(": ");
        output.push_str(&normalize::message(bounded(message, 768)));

        let frames = selected.map_or(&[][..], |(_, segment)| segment.frames.as_slice());
        for frame in frames.iter().take(MAX_FRAMES) {
            output.push('\n');
            if let Some(function) = frame.function {
                output.push_str(bounded(function, 512));
            }
            if let Some(file) = frame.file {
                output.push_str(" @ ");
                output.push_str(&normalize::source_file(bounded(file, 512)));
            }
            if frame.function.is_none()
                && frame.file.is_none()
                && let Some(module) = frame.module
            {
                output.push_str(bounded(module, 512));
            }
        }
        // A failed or header-only parse must not discard the original evidence.
        if frames.is_empty() && !stack.trim().is_empty() {
            output.push_str("\nraw: ");
            output.push_str(bounded(stack, MAX_FIELD_BYTES));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(language: &str, kind: &str, message: &str, stack: &str) -> Input {
        Input {
            language: language.into(),
            error_type: kind.into(),
            error_message: message.into(),
            stacktrace: stack.into(),
            mapped_stacktrace: None,
        }
    }
    #[test]
    fn omitted_and_blank_languages_match_ingestion_default() {
        let row =
            serde_json::json!({"error_type":"Error", "error_message":"failed", "stacktrace":WORLD});
        let omitted: Input = serde_json::from_value(row).unwrap();
        assert_eq!(
            omitted.text(),
            input("java", "Error", "failed", WORLD).text()
        );
        assert_eq!(omitted.text(), input("  ", "Error", "failed", WORLD).text());
    }

    #[test]
    fn truncated_parse_keeps_raw_evidence() {
        let stack = "at app.Main.run(Main.java:1)\n".repeat(257);
        assert!(
            input("java", "Error", "failed", &stack)
                .text()
                .contains("raw: ")
        );
    }

    // Changing these expectations requires a reviewed embedding version change.
    #[test]
    fn preparation_matches_versioned_contract() {
        #[derive(Deserialize)]
        struct Corpus {
            model_version: String,
            cases: Vec<Case>,
        }
        #[derive(Deserialize)]
        struct Case {
            name: String,
            input: Input,
            expected: String,
        }
        let corpus: Corpus =
            serde_json::from_str(include_str!("../tests/fixtures/preparation-v3.json")).unwrap();
        assert_eq!(corpus.model_version, crate::model::VERSION);
        for case in corpus.cases {
            assert_eq!(
                case.input.text(),
                case.expected,
                "{}: preparation changed; do not overwrite outputs under an existing model version",
                case.name
            );
        }
    }

    const WORLD: &str = "java.base/sun.nio.fs.UnixException.translateToIOException(UnixException.java:92)\njava.base/java.nio.file.Files.newInputStream(Files.java:154)\nworlds-3.12.2-mc1.21.8-all.jar//net.thenextlvl.nbt.NBTInputStream.create(NBTInputStream.java:200)\nworlds-3.12.2-mc1.21.8-all.jar//net.thenextlvl.worlds.view.PaperLevelView.getLevelDataFile(PaperLevelView.java:125)";
    #[test]
    fn worlds_regression_same_text_despite_directory_jar_and_line_changes() {
        let a = input(
            "java",
            "java.nio.file.NoSuchFileException",
            "./Sword/level.dat",
            WORLD,
        );
        let b = input(
            "java",
            "java.nio.file.NoSuchFileException",
            "./library/level.dat",
            &WORLD
                .replace("3.12.2-mc1.21.8", "4.0.0-mc1.22")
                .replace(":125", ":999")
                .replace("java.base/", "java.base@21/"),
        );
        assert_eq!(a.text(), b.text());
        assert!(a.text().contains("<path>/level.dat"));
        assert!(a.text().contains("PaperLevelView.getLevelDataFile"));
        let other_file = input("java", &a.error_type, "./library/config.yml", WORLD);
        assert_ne!(a.text(), other_file.text());
        let other_caller = input(
            "java",
            &a.error_type,
            &a.error_message,
            &WORLD.replace("getLevelDataFile", "saveLevelDataFile"),
        );
        assert_ne!(a.text(), other_caller.text());
    }
    #[test]
    fn shared_parser_removes_source_coordinates_in_all_languages() {
        for (language, stack) in [
            (
                "javascript",
                "Error: failed\nat load (/srv/app/main.js:12:34)",
            ),
            (
                "python",
                "Traceback (most recent call last):\n  File \"/srv/app/main.py\", line 12, in load\n    load()\nValueError: failed",
            ),
            (
                "rust",
                "thread 'main' panicked at src/main.rs:12:34:\nfailed\nstack backtrace:\n 0: app::load\n at src/main.rs:12:34",
            ),
            (
                "go",
                "panic: failed\ngoroutine 1 [running]:\napp.load()\n /app/main.go:12 +0x1",
            ),
            (
                "php",
                "PHP Fatal error: Uncaught Error: failed in /srv/main.php:12\n#0 /srv/main.php(12): App->load()",
            ),
            (
                "swift",
                "Thread 0 crashed:\n0 0x1 App.load() + 41 in demo at /app/main.swift:12:34",
            ),
        ] {
            let a = input(language, "Error", "failed", stack);
            let b = input(
                language,
                "Error",
                "failed",
                &stack.replace("12", "999").replace(":34", ":87"),
            );
            assert!(!a.text().contains("raw:"), "{language}: {}", a.text());
            assert_eq!(a.text(), b.text(), "{language}");
        }
    }
    #[test]
    fn causes_keep_messages_and_signatures_without_suppressed_errors() {
        let stack = "java.lang.RuntimeException: wrapper\n at app.Main.run(Main.java:1)\n Suppressed: java.lang.Error: suppressed\n  at app.Close.close(Close.java:2)\n Caused by: java.lang.Error: nested suppressed\n  at app.Close.nested(Close.java:3)\nCaused by: java.lang.NoSuchMethodError: api.run(int)\n at app-1.jar//app.Main.call(Main.java:12)";
        let a = input("java", "java.lang.RuntimeException", "wrapper", stack);
        assert_eq!(
            a.text(),
            "java.lang.NoSuchMethodError: api.run(int)\napp.Main.call @ Main.java"
        );
        let b = input(
            "java",
            "java.lang.RuntimeException",
            "wrapper",
            &stack.replace("api.run(int)", "api.run(String)"),
        );
        assert_ne!(a.text(), b.text());
    }
    #[test]
    fn python_chain_preserves_terminal_message() {
        let stack = "Traceback (most recent call last):\n  File \"io.py\", line 1, in load\n    open()\nFileNotFoundError: [Errno 2] No such file or directory: './Sword/level.dat'\n\nThe above exception was the direct cause of the following exception:\n\nTraceback (most recent call last):\n  File \"app.py\", line 2, in run\n    load()\nRuntimeError: wrapper";
        let a = input("python", "RuntimeError", "wrapper", stack);
        let b = input(
            "python",
            "RuntimeError",
            "wrapper",
            &stack.replace("Sword", "library"),
        );
        assert_eq!(a.text(), b.text());
        assert!(a.text().starts_with("FileNotFoundError: [Errno 2]"));
    }
    #[test]
    fn mappings_and_unknown_stacks_preserve_evidence() {
        let mut a = input("java", "Error", "failed", "at a.b(a.java:1)");
        let original = a.text();
        a.mapped_stacktrace = Some(" \n".into());
        assert_eq!(original, a.text());
        a.mapped_stacktrace = Some("at app.Main.run(Main.java:42)".into());
        assert!(a.text().contains("app.Main.run"));
        assert!(!a.text().contains("a.b"));
        assert_ne!(
            input("unknown", "Error", "failed", "first").text(),
            input("unknown", "Error", "failed", "second").text()
        );
        assert!(
            input("java", "Error", "failed", &"🦀".repeat(300_000))
                .text()
                .len()
                < 5000
        );
    }
}
