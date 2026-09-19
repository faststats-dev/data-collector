use crate::Mapping;
use std::{collections::HashMap, fmt::Write, str::Utf8Error};

/// A parsed ProGuard or R8 mapping.
pub struct ProguardMapping {
    classes: HashMap<String, ClassMapping>,
    source_files: HashMap<String, String>,
}

#[derive(Default)]
struct ClassMapping {
    original: String,
    file: Option<String>,
    synthetic: bool,
    methods: HashMap<String, Methods>,
}

#[derive(Default)]
struct Methods {
    entries: Vec<MethodMapping>,
    outline: bool,
    synthetic: bool,
    overlapping: bool,
    unique: bool,
    catch_all: bool,
}

impl Methods {
    fn prepare(&mut self) {
        // Missing line numbers should not require scanning every range per frame.
        self.unique = true;
        self.catch_all = true;
        for method in &self.entries {
            self.unique &= method.original == self.entries[0].original;
            self.catch_all &= matches!(method.range, Some((0, end)) if end >= 65535);
            if let Some(metadata) = &method.metadata {
                self.outline |= metadata.outline;
                self.synthetic |= metadata.synthetic;
            }
        }
        // Stable sorting preserves the innermost-to-outermost order of inlinees.
        self.entries.sort_by_key(|m| m.range);
        self.overlapping = self.entries.windows(2).any(|pair| {
            let (Some(left), Some(right)) = (pair[0].range, pair[1].range) else {
                return false;
            };
            left != right && left.1 >= right.0
        });
    }

    fn select(&self, line: Option<u32>) -> &[MethodMapping] {
        let Some(line) = line else {
            return if self.unique { &self.entries[..1] } else { &[] };
        };
        let end = self
            .entries
            .partition_point(|m| m.range.is_none_or(|(start, _)| start <= line));
        let candidates = &self.entries[..end];
        let contains = |m: &&MethodMapping| m.range.is_some_and(|(a, b)| (a..=b).contains(&line));
        let candidate = if self.overlapping {
            let mut matches = candidates.iter().filter(contains);
            match matches.next() {
                Some(first) if matches.any(|m| m.range != first.range) => return &[],
                first => first,
            }
        } else {
            candidates.last().filter(contains)
        };
        if let Some(method) = candidate {
            let start = candidates.partition_point(|m| m.range < method.range);
            let end = candidates.partition_point(|m| m.range <= method.range);
            return &candidates[start..end];
        }
        let defaults = &self.entries[..self.entries.partition_point(|m| m.range.is_none())];
        if defaults
            .first()
            .is_some_and(|first| defaults.iter().all(|m| m.original == first.original))
        {
            return defaults;
        }
        &[]
    }
}

struct MethodMapping {
    original: String,
    range: Option<(u32, u32)>,
    original_range: Option<(u32, u32)>,
    metadata: Option<Box<Metadata>>,
}

// Boxed on MethodMapping because most methods have no R8 metadata.
#[derive(Default)]
struct Metadata {
    synthetic: bool,
    outline: bool,
    positions: HashMap<u32, u32>,
    rewrites: Vec<Rewrite>,
}

struct Rewrite {
    exceptions: Vec<String>,
    remove: usize,
}
#[derive(Default)]
struct Context<'a> {
    exception: Option<&'a str>,
    first_frame: bool,
    outline: Option<u32>,
}

struct Frame<'a> {
    prefix: &'a str,
    module: &'a str,
    class: &'a str,
    method: &'a str,
    file: &'a str,
    line: Option<u32>,
    tail: &'a str,
}

impl<'a> Frame<'a> {
    fn parse(body: &'a str) -> Option<Self> {
        let rest = body.trim_start();
        let prefix = &body[..body.len() - rest.len()];
        let (qualified, location) = rest.strip_prefix("at ")?.split_once('(')?;
        let (location, tail) = location.split_once(')')?;
        let (module, qualified) = container_prefix(qualified);
        let (class, method) = qualified.rsplit_once('.')?;
        let (file, line) = if let Some((file, line)) = location.rsplit_once(':')
            && let Ok(line) = line.parse()
        {
            (file, Some(line))
        } else {
            (location, None)
        };
        Some(Self {
            prefix,
            module,
            class,
            method,
            file,
            line,
            tail,
        })
    }
}

impl ProguardMapping {
    /// Unknown records and malformed metadata are ignored. Ambiguity is preserved.
    pub fn parse(input: &str) -> Self {
        let mut classes = HashMap::new();
        parse_into(input, &mut classes);
        Self::from_classes(classes)
    }

    pub fn parse_many_bytes(parts: &[Vec<u8>]) -> Result<Self, Utf8Error> {
        let mut classes = HashMap::new();
        for part in parts {
            parse_into(std::str::from_utf8(part)?, &mut classes);
        }
        Ok(Self::from_classes(classes))
    }

    fn from_classes(mut classes: HashMap<String, ClassMapping>) -> Self {
        for class in classes.values_mut() {
            for methods in class.methods.values_mut() {
                methods.prepare();
            }
        }
        let source_files = classes
            .values()
            .filter_map(|class| Some((class.original.clone(), class.file.clone()?)))
            .collect();
        Self {
            classes,
            source_files,
        }
    }

    fn retrace_frame(
        &self,
        frame: &Frame<'_>,
        class: &ClassMapping,
        ending: &str,
        context: &mut Context<'_>,
        out: &mut String,
    ) {
        let first = std::mem::take(&mut context.first_frame);
        let methods = class.methods.get(frame.method);
        if methods.is_some_and(|m| m.outline) {
            context.outline = frame.line;
            return;
        }
        let mut line = frame.line;
        if let Some(methods) = methods {
            if let Some(position) = context.outline.take() {
                line = methods
                    .select(line)
                    .iter()
                    .find_map(|m| m.metadata.as_ref()?.positions.get(&position).copied())
                    .or(line);
            }
            if line.is_none() && frame.file != "Native Method" && methods.catch_all {
                line = Some(0);
            }
        }
        let selected = methods.map_or(&[][..], |m| m.select(line));
        if selected.is_empty() {
            self.write_frame(frame, class, None, frame.line, out);
            out.push_str(ending);
            return;
        }
        let mut skip = 0usize;
        if first {
            for method in selected {
                let Some(metadata) = &method.metadata else {
                    continue;
                };
                for rule in &metadata.rewrites {
                    if rule
                        .exceptions
                        .iter()
                        .all(|c| context.exception == Some(c.as_str()))
                    {
                        skip = skip.saturating_add(rule.remove);
                    }
                }
            }
        }
        if skip > selected.len() {
            skip = 0;
        }
        let synthetic = class.synthetic || methods.is_some_and(|m| m.synthetic);
        let mut written = false;
        for (index, method) in selected.iter().enumerate().skip(skip) {
            if (synthetic && index + 1 == selected.len())
                || method.metadata.as_ref().is_some_and(|m| m.synthetic)
            {
                continue;
            }
            if written {
                out.push_str(if ending.is_empty() { "\n" } else { ending });
            }
            self.write_frame(frame, class, Some(method), line, out);
            written = true;
        }
        if written {
            out.push_str(ending);
        }
    }

    fn write_frame(
        &self,
        frame: &Frame<'_>,
        class: &ClassMapping,
        method: Option<&MethodMapping>,
        line: Option<u32>,
        out: &mut String,
    ) {
        let original = method.map_or(frame.method, |m| m.original.as_str());
        let (owner, name) = original
            .rsplit_once('.')
            .unwrap_or((&class.original, original));
        let source = if frame.file == "Native Method" {
            frame.file
        } else if owner == class.original {
            class.file.as_deref().unwrap_or(frame.file)
        } else {
            self.source_files
                .get(owner)
                .map_or("SourceFile", String::as_str)
        };
        write!(out, "{}at {}{owner}.{name}(", frame.prefix, frame.module).unwrap();
        if source.is_empty() || source == "SourceFile" {
            let simple = owner
                .rsplit('.')
                .next()
                .unwrap_or(owner)
                .split('$')
                .next()
                .unwrap_or(owner);
            out.push_str(simple);
            out.push_str(".java");
        } else {
            out.push_str(source);
        }
        if let Some(mut line) = line {
            if let Some(method) = method
                && let Some((start, end)) = method.original_range
            {
                let offset = line.saturating_sub(method.range.map_or(line, |r| r.0));
                line = start.saturating_add(offset).min(end);
            }
            if line != 0 {
                write!(out, ":{line}").unwrap();
            }
        }
        out.push(')');
        out.push_str(frame.tail);
    }
}

impl Mapping for ProguardMapping {
    fn apply(&self, stacktrace: &str) -> Option<String> {
        let mut out = crate::Rewritten::new(stacktrace);
        let mut context = Context::default();
        let mut offset = 0;
        for line in stacktrace.split_inclusive('\n') {
            let range = offset..offset + line.len();
            offset = range.end;
            let body = line.trim_end_matches(['\r', '\n']);
            let ending = &line[body.len()..];
            if let Some(frame) = Frame::parse(body) {
                if let Some(class) = self.classes.get(frame.class) {
                    self.retrace_frame(&frame, class, ending, &mut context, out.replace(range));
                } else {
                    context.first_frame = false;
                    context.outline = None;
                }
                continue;
            }

            context.outline = None;
            context.first_frame = false;
            let trimmed = body.trim_start();
            let (before, rest) = exception_parts(trimmed);
            let end = rest.find([':', ' ']).unwrap_or(rest.len());
            let (module, name) = container_prefix(&rest[..end]);
            let class = self.classes.get(name);
            if name.is_empty() || (before.is_empty() && !name.contains('.') && class.is_none()) {
                continue;
            }
            context.exception = Some(class.map_or(name, |c| &c.original));
            context.first_frame = true;
            if let Some(class) = class {
                let indent = &body[..body.len() - trimmed.len()];
                write!(
                    out.replace(range),
                    "{indent}{before}{module}{}{}{ending}",
                    class.original,
                    &rest[end..]
                )
                .unwrap();
            }
        }
        out.finish()
    }
}

fn exception_parts(line: &str) -> (&str, &str) {
    for prefix in ["Caused by: ", "Suppressed: "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return (prefix, rest);
        }
    }
    if let Some((_, rest)) = line
        .strip_prefix("Exception in thread \"")
        .and_then(|s| s.split_once("\" "))
    {
        return (&line[..line.len() - rest.len()], rest);
    }
    ("", line)
}

fn container_prefix(name: &str) -> (&str, &str) {
    name.rfind('/')
        .map_or(("", name), |idx| (&name[..=idx], &name[idx + 1..]))
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct Record {
    id: String,
    version: String,
    #[serde(rename = "fileName")]
    file: Option<String>,
    positions: HashMap<u32, u32>,
    conditions: Option<Vec<String>>,
    actions: Option<Vec<String>>,
}

impl Record {
    fn apply(self, class: &mut ClassMapping, target: &MetadataTarget<'_>, version: (u32, u32)) {
        if self.id == "sourceFile" {
            if matches!(target, MetadataTarget::Class) && self.file.is_some() {
                class.file = self.file;
            }
            return;
        }
        if !((1, 0)..=(2, 2)).contains(&version) {
            return;
        }
        let method = match target {
            MetadataTarget::Method(name) => class
                .methods
                .get_mut(*name)
                .and_then(|m| m.entries.last_mut()),
            _ => None,
        };
        if self.id == "com.android.tools.r8.synthesized" {
            if let Some(method) = method {
                method.metadata.get_or_insert_default().synthetic = true;
            } else if matches!(target, MetadataTarget::Class) {
                class.synthetic = true;
            }
            return;
        }
        let Some(method) = method.filter(|_| version >= (2, 0)) else {
            return;
        };
        match self.id.as_str() {
            "com.android.tools.r8.outline" => {
                method.metadata.get_or_insert_default().outline = true
            }
            "com.android.tools.r8.outlineCallsite" => method
                .metadata
                .get_or_insert_default()
                .positions
                .extend(self.positions),
            "com.android.tools.r8.rewriteFrame" => {
                if let (Some(conditions), Some(actions)) = (self.conditions, self.actions)
                    && let Some(rule) = Rewrite::parse(&conditions, &actions)
                {
                    method.metadata.get_or_insert_default().rewrites.push(rule);
                }
            }
            _ => {}
        }
    }
}

impl Rewrite {
    fn parse(conditions: &[String], actions: &[String]) -> Option<Self> {
        let mut exceptions = Vec::with_capacity(conditions.len());
        for condition in conditions {
            let class = condition.strip_prefix("throws(L")?.strip_suffix(";)")?;
            exceptions.push(class.replace('/', "."));
        }
        let mut remove = 0usize;
        for action in actions {
            let count = action
                .strip_prefix("removeInnerFrames(")?
                .strip_suffix(')')?;
            remove = remove.checked_add(count.parse().ok()?)?;
        }
        Some(Self { exceptions, remove })
    }
}

// Fields and malformed members deliberately block metadata attachment.
enum MetadataTarget<'a> {
    Class,
    Method(&'a str),
    Ignored,
}

fn parse_into(input: &str, classes: &mut HashMap<String, ClassMapping>) {
    let mut current: Option<&mut ClassMapping> = None;
    let mut target = MetadataTarget::Class;
    let mut metadata_version = (0, 0);
    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let Ok(record) = serde_json::from_str::<Record>(comment.trim()) else {
                continue;
            };
            if record.id == "com.android.tools.r8.mapping" {
                metadata_version = record
                    .version
                    .split_once('.')
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                    .unwrap_or((u32::MAX, 0));
                continue;
            }
            if let Some(class) = current.as_deref_mut() {
                record.apply(class, &target, metadata_version);
            }
            continue;
        }
        if !raw.starts_with([' ', '\t']) {
            target = MetadataTarget::Class;
            current = None;
            let Some((original, obfuscated)) =
                line.strip_suffix(':').and_then(|s| s.split_once(" -> "))
            else {
                continue;
            };
            if original.is_empty() || obfuscated.is_empty() {
                continue;
            }
            let class = classes.entry(obfuscated.to_owned()).or_default();
            // Conflicting artifacts must not merge methods from different classes.
            if !class.original.is_empty() && class.original != original {
                continue;
            }
            if class.original.is_empty() {
                class.original = original.to_owned();
            }
            current = Some(class);
            continue;
        }
        target = MetadataTarget::Ignored;
        if let Some(class) = current.as_deref_mut()
            && let Some((name, method)) = parse_method(line)
        {
            if let Some(methods) = class.methods.get_mut(name) {
                methods.entries.push(method);
            } else {
                class.methods.insert(
                    name.to_owned(),
                    Methods {
                        entries: vec![method],
                        ..Methods::default()
                    },
                );
            }
            target = MetadataTarget::Method(name);
        }
    }
}

fn parse_method(line: &str) -> Option<(&str, MethodMapping)> {
    let (mut signature, obfuscated) = line.rsplit_once(" -> ")?;
    if obfuscated.is_empty() {
        return None;
    }
    let range = if signature.as_bytes().first()?.is_ascii_digit() {
        let (start, rest) = signature.split_once(':')?;
        let (end, rest) = rest.split_once(':')?;
        signature = rest;
        Some(parse_range(start, end)?)
    } else {
        None
    };
    let (before, after) = signature.split_once('(')?;
    let (_, name) = before.rsplit_once(' ')?;
    if name.is_empty() {
        return None;
    }
    let (_, suffix) = after.split_once(')')?;
    let original_range = if let Some(lines) = suffix.strip_prefix(':') {
        let (start, end) = lines.split_once(':').unwrap_or((lines, lines));
        Some(parse_range(start, end)?)
    } else if suffix.is_empty() {
        None
    } else {
        return None;
    };
    Some((
        obfuscated,
        MethodMapping {
            original: name.to_owned(),
            range,
            original_range,
            metadata: None,
        },
    ))
}

fn parse_range(start: &str, end: &str) -> Option<(u32, u32)> {
    let range = (start.parse().ok()?, end.parse().ok()?);
    (range.0 <= range.1).then_some(range)
}
