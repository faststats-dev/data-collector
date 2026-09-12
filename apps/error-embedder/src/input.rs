use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
pub struct Input {
    pub language: String,
    pub error_type: String,
    pub error_message: String,
    pub stacktrace: String,
    pub mapped_stacktrace: Option<String>,
}

static FRAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:at\s+)?([^\s]+)\([^)]*\)$").unwrap());
static MORE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.\.\. (\d+) more$").unwrap());
static LAMBDA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"lambda\$([^($]+)\$\d+").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.v\d+(?:[_\.]\d+)+(?:_R\d+)?\.").unwrap());

impl Input {
    pub fn text(&self) -> String {
        let mut header = format!("{}: {}", self.error_type, self.error_message);
        let mut frames = Vec::<String>::new();
        let mut parent = Vec::<String>::new();
        let java = matches!(
            self.language.to_ascii_lowercase().as_str(),
            "java" | "kotlin" | "jvm" | "scala"
        );
        let stacktrace = self
            .mapped_stacktrace
            .as_deref()
            .unwrap_or(&self.stacktrace);
        if java {
            for (i, line) in stacktrace.lines().enumerate() {
                let line = line.trim();
                if let Some(cause) = line.strip_prefix("Caused by: ") {
                    parent = std::mem::take(&mut frames);
                    header = cause.to_owned();
                } else if i == 0
                    && line
                        .split(':')
                        .next()
                        .is_some_and(|s| s.ends_with("Exception") || s.ends_with("Error"))
                {
                    header = line.to_owned();
                } else if let Some(caps) = MORE.captures(line) {
                    let n = caps[1].parse::<usize>().unwrap_or(0).min(parent.len());
                    frames.extend_from_slice(&parent[parent.len() - n..]);
                } else if let Some(caps) = FRAME.captures(line) {
                    let name = &caps[1];
                    let name = name.split_once(".jar//").map_or(name, |(_, frame)| frame);
                    frames.push(LAMBDA.replace_all(name, "lambda$$${1}$$N").into_owned());
                }
            }
            if frames.is_empty() {
                frames = parent;
            }
        } else {
            // Other languages retain their original frames; Java-specific cleanup
            // must not erase Python/Rust/JS throw sites.
            frames = stacktrace.lines().take(16).map(str::to_owned).collect();
        }
        if let Some((message, _)) = header.split_once(", context=[") {
            header = message.to_owned();
        }
        let message = header.split_once(':').map_or("", |(_, m)| m.trim());
        let generic = message.is_empty() || message.contains("Could not pass event ");
        let mut app: Vec<_> = frames
            .iter()
            .filter(|f| {
                ![
                    "java.",
                    "jdk.",
                    "sun.",
                    "org.bukkit.",
                    "io.papermc.",
                    "net.minecraft.",
                    "co.aikar.",
                    "runtime.scheduler.",
                ]
                .iter()
                .any(|p| f.starts_with(p))
            })
            .take(3)
            .collect();
        if app.is_empty() {
            app.extend(frames.iter().take(3));
        }
        let normalize = |f: &str| VERSION.replace_all(f, ".version.").into_owned();
        let selected = if java && !generic {
            frames.first().cloned().unwrap_or_default()
        } else {
            app.iter()
                .take(3)
                .map(|f| normalize(f))
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!("{header}\n{selected}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(stack: &str) -> Input {
        Input {
            language: "java".into(),
            error_type: "java.lang.RuntimeException".into(),
            error_message: "wrapper".into(),
            stacktrace: stack.into(),
            mapped_stacktrace: None,
        }
    }
    #[test]
    fn causes_preserve_api_signatures_and_remove_frame_noise() {
        let p=input("java.lang.RuntimeException: wrapper\nCaused by: java.lang.NoSuchMethodError: api.run(int)\n at app-1.jar//app.Main.call(Main.java:12)").text();
        assert_eq!(
            p,
            "java.lang.NoSuchMethodError: api.run(int)\napp.Main.call"
        );
    }
    #[test]
    fn generic_helpers_retain_application_origin() {
        let p=input("java.lang.NullPointerException\n at java.util.Objects.requireNonNull(Objects.java:1)\n at app.Main.load(Main.java:2)").text();
        assert!(p.contains("app.Main.load"));
    }
    #[test]
    fn embedding_uses_the_mapped_throw_site() {
        let mut error = input("at a.b(a.java:1)");
        error.mapped_stacktrace = Some("at app.Main.load(Main.java:42)".into());
        assert!(error.text().contains("app.Main.load"));
        assert!(!error.text().contains("a.b"));
    }
}
