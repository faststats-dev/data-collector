#![doc = include_str!("../README.md")]
mod javascript;
mod proguard;

pub use javascript::{JavaScriptMapping, ReactNativeMapping, SourceMap};
pub use proguard::ProguardMapping;

/// A parsed mapping that can be reused across stacktraces.
pub trait Mapping {
    /// Returns the rewritten stacktrace, or `None` if nothing changed.
    /// Unrecognized frames and text are preserved.
    fn apply(&self, stacktrace: &str) -> Option<String>;
}

/// Copies unchanged spans only when a replacement is actually produced.
struct Rewritten<'a> {
    original: &'a str,
    output: String,
    copied: usize,
}

impl<'a> Rewritten<'a> {
    fn new(original: &'a str) -> Self {
        Self {
            original,
            output: String::new(),
            copied: 0,
        }
    }

    fn replace(&mut self, range: std::ops::Range<usize>) -> &mut String {
        if self.output.capacity() == 0 {
            self.output.reserve(self.original.len());
        }
        self.output
            .push_str(&self.original[self.copied..range.start]);
        self.copied = range.end;
        &mut self.output
    }

    fn finish(mut self) -> Option<String> {
        if self.copied == 0 {
            return None;
        }
        self.output.push_str(&self.original[self.copied..]);
        (self.output != self.original).then_some(self.output)
    }
}
