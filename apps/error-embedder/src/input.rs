use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub struct Input {
    pub project_id: Uuid,
    pub language: String,
    pub error_type: String,
    pub error_message: String,
    pub stacktrace: String,
}

pub struct Prepared {
    pub text: String,
    pub root_type: String,
    pub signature: String,
    pub origin: String,
    pub generic: bool,
    pub isolated: bool,
}

static FRAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:at\s+)?([^\s]+)\([^)]*\)$").unwrap());
static MORE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.\.\. (\d+) more$").unwrap());
static LAMBDA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"lambda\$([^($]+)\$\d+").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.v\d+(?:[_\.]\d+)+(?:_R\d+)?\.").unwrap());

impl Input {
    pub fn hash(&self) -> String {
        let mut digest = Sha256::new();
        for part in [
            &self.language,
            &self.error_type,
            &self.error_message,
            &self.stacktrace,
        ] {
            digest.update(format!("{}:", part.len()).as_bytes());
            digest.update(part.as_bytes());
        }
        hex::encode(digest.finalize())
    }

    pub fn prepare(&self) -> Prepared {
        let mut header = format!("{}: {}", self.error_type, self.error_message);
        let mut frames = Vec::<String>::new();
        let mut parent = Vec::<String>::new();
        let java = matches!(
            self.language.to_ascii_lowercase().as_str(),
            "java" | "kotlin" | "jvm" | "scala"
        );
        if java {
            for (i, line) in self.stacktrace.lines().enumerate() {
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
            frames = self
                .stacktrace
                .lines()
                .take(16)
                .map(str::to_owned)
                .collect();
        }
        if let Some((message, _)) = header.split_once(", context=[") {
            header = message.to_owned();
        }
        let root_type = header.split(':').next().unwrap_or("").to_owned();
        let message = header.split_once(':').map_or("", |(_, m)| m.trim());
        let generic = message.is_empty() || message.contains("Could not pass event ");
        let isolated = self.stacktrace.is_empty()
            || root_type.ends_with("InvocationTargetException")
            || message.contains("Could not pass event ");
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
        let origin = app.first().map(|f| normalize(f)).unwrap_or_default();
        let selected = if java && !generic {
            frames.first().cloned().unwrap_or_default()
        } else {
            app.iter()
                .take(3)
                .map(|f| normalize(f))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let signature = if [
            "NoSuchMethodError",
            "NoSuchFieldError",
            "ClassNotFoundException",
        ]
        .iter()
        .any(|s| root_type.ends_with(s))
        {
            header.clone()
        } else {
            String::new()
        };
        Prepared {
            text: format!("{header}\n{selected}"),
            root_type,
            signature,
            origin,
            generic,
            isolated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(stack: &str) -> Input {
        Input {
            project_id: Uuid::nil(),
            language: "java".into(),
            error_type: "java.lang.RuntimeException".into(),
            error_message: "wrapper".into(),
            stacktrace: stack.into(),
        }
    }
    #[test]
    fn causes_preserve_api_signatures_and_remove_frame_noise() {
        let p=input("java.lang.RuntimeException: wrapper\nCaused by: java.lang.NoSuchMethodError: api.run(int)\n at app-1.jar//app.Main.call(Main.java:12)").prepare();
        assert_eq!(
            p.text,
            "java.lang.NoSuchMethodError: api.run(int)\napp.Main.call"
        );
        assert_eq!(p.signature, "java.lang.NoSuchMethodError: api.run(int)");
    }
    #[test]
    fn hash_includes_message_and_unambiguous_utf8_lengths() {
        let mut a = input("same");
        let h = a.hash();
        a.error_message = "different".into();
        assert_ne!(h, a.hash());
        a.error_message = "é:日".into();
        assert_eq!(a.hash().len(), 64);
    }
    #[test]
    fn generic_helpers_retain_application_origin() {
        let p=input("java.lang.NullPointerException\n at java.util.Objects.requireNonNull(Objects.java:1)\n at app.Main.load(Main.java:2)").prepare();
        assert!(p.generic);
        assert_eq!(p.origin, "app.Main.load");
        assert!(p.text.contains("app.Main.load"));
    }
}
