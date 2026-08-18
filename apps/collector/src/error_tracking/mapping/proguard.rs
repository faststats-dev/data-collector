use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::str::Utf8Error;

pub(super) struct ProguardMapping {
    classes: HashMap<String, ClassMapping>,
}

struct ClassMapping {
    original_name: String,
    file_name: Option<String>,
    methods: Vec<MethodMapping>,
}

struct MethodMapping {
    original_name: String,
    obfuscated_name: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

#[derive(serde::Deserialize)]
struct MappingMetadata {
    #[serde(rename = "fileName")]
    file_name: Option<String>,
}

impl ProguardMapping {
    #[cfg(test)]
    fn parse(input: &str) -> Self {
        let mut classes = HashMap::new();
        parse_into(input, &mut classes);
        Self { classes }
    }

    pub(super) fn parse_many_bytes(parts: &[Vec<u8>]) -> Result<Self, Utf8Error> {
        let mut classes = HashMap::new();
        for part in parts {
            parse_into(std::str::from_utf8(part)?, &mut classes);
        }
        Ok(Self { classes })
    }

    pub(super) fn retrace(&self, stacktrace: &str) -> String {
        let mut out = String::with_capacity(stacktrace.len());

        let mut first = true;
        for line in stacktrace.lines() {
            if !first {
                out.push('\n');
            }
            first = false;
            self.retrace_line_into(line, &mut out);
        }

        if stacktrace.ends_with('\n') {
            out.push('\n');
        }

        out
    }

    fn retrace_line_into(&self, line: &str, out: &mut String) {
        let trimmed = line.trim_start();
        let prefix_len = line.len() - trimmed.len();
        let prefix = &line[..prefix_len];

        if let Some(rest) = trimmed.strip_prefix("at ") {
            self.retrace_stack_frame(prefix, rest, out);
        } else {
            self.retrace_exception_line_into(line, out);
        }
    }

    fn retrace_stack_frame(&self, prefix: &str, rest: &str, out: &mut String) {
        let Some(paren_start) = rest.find('(') else {
            push_java_frame(out, prefix, rest);
            return;
        };

        let qualified = &rest[..paren_start];
        let location = &rest[paren_start..];
        let (class_prefix, qualified) = split_container_prefix(qualified);

        let Some(dot_pos) = qualified.rfind('.') else {
            push_java_frame(out, prefix, rest);
            return;
        };

        let obf_class = &qualified[..dot_pos];
        let obf_method = &qualified[dot_pos + 1..];

        let Some(class) = self.classes.get(obf_class) else {
            push_java_frame(out, prefix, rest);
            return;
        };

        let line_num = parse_stacktrace_line_number(location);
        let resolved_method = self.resolve_method(class, obf_method, line_num);
        let method_name = resolved_method
            .map(|m| m.original_name.as_str())
            .unwrap_or(obf_method);
        let source_file = class.file_name.as_deref().unwrap_or("Unknown Source");

        out.reserve(
            prefix.len()
                + 4
                + class_prefix.len()
                + class.original_name.len()
                + method_name.len()
                + source_file.len()
                + 16,
        );
        out.push_str(prefix);
        out.push_str("at ");
        out.push_str(class_prefix);
        out.push_str(&class.original_name);
        out.push('.');
        out.push_str(method_name);
        out.push('(');
        out.push_str(source_file);
        if let Some(n) = line_num {
            out.push(':');
            push_u32(out, n);
        }
        out.push(')');
    }

    fn retrace_exception_line_into(&self, line: &str, out: &mut String) {
        let trimmed = line.trim_start();
        let prefix_len = line.len() - trimmed.len();
        let prefix = &line[..prefix_len];

        let (before_class, class_and_rest) = if let Some(rest) = trimmed.strip_prefix("Caused by: ")
        {
            ("Caused by: ", rest)
        } else {
            ("", trimmed)
        };

        let (class_part, suffix) = class_and_rest
            .split_once(": ")
            .map(|(c, m)| (c, Some(m)))
            .unwrap_or((class_and_rest, None));

        let (class_prefix, obf_class) = split_container_prefix(class_part);

        if let Some(class) = self.classes.get(obf_class) {
            out.reserve(
                prefix.len()
                    + before_class.len()
                    + class_prefix.len()
                    + class.original_name.len()
                    + suffix.map(str::len).unwrap_or(0)
                    + 2,
            );
            out.push_str(prefix);
            out.push_str(before_class);
            out.push_str(class_prefix);
            out.push_str(&class.original_name);

            if let Some(message) = suffix {
                out.push_str(": ");
                out.push_str(message);
            }
        } else {
            out.push_str(line);
        }
    }

    fn resolve_method<'a>(
        &'a self,
        class: &'a ClassMapping,
        obf_method: &str,
        line: Option<u32>,
    ) -> Option<&'a MethodMapping> {
        let mut fallback = None;
        for method in &class.methods {
            if method.obfuscated_name != obf_method {
                continue;
            }
            fallback.get_or_insert(method);
            if matches!(
                (line, method.start_line, method.end_line),
                (Some(line), Some(start), Some(end)) if (start..=end).contains(&line)
            ) {
                return Some(method);
            }
        }
        fallback
    }
}

fn parse_into(input: &str, classes: &mut HashMap<String, ClassMapping>) {
    let mut current_class: Option<(String, ClassMapping)> = None;

    for line in input.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('#') {
            if let Some((_, class)) = current_class.as_mut()
                && let Some(file_name) = extract_file_name(line)
            {
                class.file_name = Some(file_name);
            }
            continue;
        }

        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some((obfuscated, class)) = current_class.take() {
                insert_class(classes, obfuscated, class);
            }

            if let Some((original, obfuscated)) = parse_class_line(line) {
                current_class = Some((
                    obfuscated.to_owned(),
                    ClassMapping {
                        original_name: original.to_owned(),
                        file_name: None,
                        methods: Vec::new(),
                    },
                ));
            }
            continue;
        }

        if let Some((_, class)) = current_class.as_mut() {
            parse_member_line(line.trim(), class);
        }
    }

    if let Some((obfuscated, class)) = current_class {
        insert_class(classes, obfuscated, class);
    }
}

fn insert_class(
    classes: &mut HashMap<String, ClassMapping>,
    obfuscated_name: String,
    class: ClassMapping,
) {
    match classes.entry(obfuscated_name) {
        Entry::Occupied(mut occupied) => occupied.get_mut().merge(class),
        Entry::Vacant(vacant) => {
            vacant.insert(class);
        }
    }
}

impl ClassMapping {
    fn merge(&mut self, other: ClassMapping) {
        if self.file_name.is_none() {
            self.file_name = other.file_name;
        }
        self.methods.extend(other.methods);
    }
}

fn extract_file_name(line: &str) -> Option<String> {
    let json_start = line.find('{')?;
    serde_json::from_str::<MappingMetadata>(&line[json_start..])
        .ok()?
        .file_name
}

fn parse_class_line(line: &str) -> Option<(&str, &str)> {
    let line = line.strip_suffix(':')?;
    let (original, obfuscated) = line.split_once(" -> ")?;
    Some((original.trim(), obfuscated.trim()))
}

fn split_container_prefix(qualified: &str) -> (&str, &str) {
    if let Some(slash_pos) = qualified.rfind('/') {
        (&qualified[..=slash_pos], &qualified[slash_pos + 1..])
    } else {
        ("", qualified)
    }
}

fn parse_stacktrace_line_number(location: &str) -> Option<u32> {
    location
        .trim_start_matches('(')
        .trim_end_matches(')')
        .rsplit_once(':')
        .and_then(|(_, num)| num.parse::<u32>().ok())
}

fn parse_member_line(line: &str, class: &mut ClassMapping) {
    let Some((original_part, obfuscated)) = line.rsplit_once(" -> ") else {
        return;
    };

    let obfuscated = obfuscated.trim();

    if let Some(method) = parse_method_with_lines(original_part) {
        class.methods.push(MethodMapping {
            original_name: method.name,
            obfuscated_name: obfuscated.to_owned(),
            start_line: Some(method.start_line),
            end_line: Some(method.end_line),
        });
        return;
    }

    if original_part.contains('(')
        && let Some(method_name) = parse_method_name(original_part)
    {
        class.methods.push(MethodMapping {
            original_name: method_name,
            obfuscated_name: obfuscated.to_owned(),
            start_line: None,
            end_line: None,
        });
    }
}

struct ParsedMethod {
    name: String,
    start_line: u32,
    end_line: u32,
}

fn parse_method_name(s: &str) -> Option<String> {
    let paren_pos = s.find('(')?;
    let before_paren = &s[..paren_pos];
    let method_name = before_paren
        .rsplit_once(' ')
        .map(|(_, name)| name)
        .unwrap_or(before_paren);
    Some(method_name.to_owned())
}

fn parse_method_with_lines(s: &str) -> Option<ParsedMethod> {
    let s = s.trim();
    let (start_str, rest) = s.split_once(':')?;
    let start_line = start_str.parse().ok()?;
    let (end_str, rest) = rest.split_once(':')?;
    let end_line = end_str.parse().ok()?;

    let paren_pos = rest.find('(')?;
    let before_paren = &rest[..paren_pos];
    let method_name = before_paren
        .rsplit_once(' ')
        .map(|(_, name)| name)
        .unwrap_or(before_paren);

    Some(ParsedMethod {
        name: method_name.to_owned(),
        start_line,
        end_line,
    })
}

fn push_java_frame(out: &mut String, prefix: &str, rest: &str) {
    out.reserve(prefix.len() + 3 + rest.len());
    out.push_str(prefix);
    out.push_str("at ");
    out.push_str(rest);
}

fn push_u32(out: &mut String, value: u32) {
    use std::fmt::Write;
    let _ = write!(out, "{value}");
}

#[cfg(test)]
mod tests {
    use super::ProguardMapping;

    #[test]
    fn retraces_r8_stacktrace() {
        let mapping = ProguardMapping::parse(
            r#"core.file.FileIO -> a.a.a:
# {"fileName":"FileIO.java","id":"sourceFile"}
    92:92:core.file.FileIO reload() -> c
"#,
        );
        let input = "\
java.lang.RuntimeException: oops
\tat a.a.a.c(SourceFile:92)";

        assert_eq!(
            mapping.retrace(input),
            "\
java.lang.RuntimeException: oops
\tat core.file.FileIO.reload(FileIO.java:92)"
        );
    }

    #[test]
    fn parse_many_bytes_combines_split_mapping_files() {
        let parts = vec![
            b"core.file.FileIO -> a.a.a:\n# {\"fileName\":\"FileIO.java\",\"id\":\"sourceFile\"}\n    92:92:core.file.FileIO reload() -> c\n".to_vec(),
            b"core.file.Validatable -> a.a.b:\n# {\"fileName\":\"Validatable.java\",\"id\":\"sourceFile\"}\n    26:26:core.file.FileIO validate() -> a_\n".to_vec(),
        ];
        let mapping = ProguardMapping::parse_many_bytes(&parts).unwrap();
        let input = "\
\tat a.a.a.c(SourceFile:92)
\tat a.a.b.a_(SourceFile:26)";

        assert_eq!(
            mapping.retrace(input),
            "\
\tat core.file.FileIO.reload(FileIO.java:92)
\tat core.file.Validatable.validate(Validatable.java:26)"
        );
    }

    #[test]
    fn parse_many_bytes_rejects_invalid_utf8() {
        assert!(ProguardMapping::parse_many_bytes(&[vec![0xff]]).is_err());
    }

    #[test]
    fn resolves_overloaded_method_by_line_in_one_pass() {
        let mapping = ProguardMapping::parse(
            r#"com.example.Service -> a:
    10:20:void first() -> b
    30:40:void second() -> b
"#,
        );

        assert_eq!(
            mapping.retrace("\tat a.b(SourceFile:35)"),
            "\tat com.example.Service.second(Unknown Source:35)"
        );
        assert_eq!(
            mapping.retrace("\tat a.b(Unknown Source)"),
            "\tat com.example.Service.first(Unknown Source)"
        );
    }
}
