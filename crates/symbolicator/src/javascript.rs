use crate::Mapping;
use sourcemap::DecodedMap;
use std::{collections::HashMap, fmt::Write, sync::Arc};

/// A decoded v3 map, including embedded index sections and Metro/Hermes metadata.
/// Parsing and lookup perform no I/O. URL-only index sections cannot be resolved.
pub struct SourceMap(DecodedMap);

impl SourceMap {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, sourcemap::Error> {
        let mut map = sourcemap::decode_slice(bytes)?;
        normalize_sources(&mut map);
        Ok(Self(map))
    }
}

// Normalize dot segments once at parse time, not for every symbolicated frame.
fn normalize_sources(map: &mut DecodedMap) {
    let regular = match map {
        DecodedMap::Index(index) => {
            for i in 0..index.get_section_count() {
                if let Some(map) = index.get_section_mut(i).and_then(|s| s.get_sourcemap_mut()) {
                    normalize_sources(map);
                }
            }
            return;
        }
        DecodedMap::Regular(map) => map,
        DecodedMap::Hermes(map) => &mut **map,
    };
    let rooted = regular
        .get_source_root()
        .is_some_and(|root| !root.is_empty());
    let changes: Vec<_> = regular
        .sources()
        .enumerate()
        .filter_map(|(i, source)| {
            let normalized =
                canonical_source(source).or_else(|| rooted.then(|| source.to_owned()))?;
            Some((i as u32, normalized))
        })
        .collect();
    // `sources()` includes sourceRoot; avoid prefixing it a second time.
    regular.set_source_root(None::<String>);
    for (i, source) in changes {
        regular.set_source(i, &source);
    }
    for i in 0..regular.get_source_count() {
        if regular.get_source_contents(i).is_some() {
            regular.set_source_contents(i, None);
        }
    }
}

fn canonical_source(source: &str) -> Option<String> {
    let (prefix, path) = if let Some((scheme, rest)) = source.split_once("://") {
        let prefix_len = scheme.len() + 3 + rest.find('/').unwrap_or(rest.len());
        (&source[..prefix_len], &source[prefix_len..])
    } else {
        ("", source)
    };
    if !path.split('/').any(|part| matches!(part, "." | "..")) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            ".." if parts.last().is_some_and(|p| *p != ".." && !p.is_empty()) => {
                parts.pop();
            }
            ".." if path.starts_with('/') && (parts.is_empty() || parts == [""]) => {}
            part => parts.push(part),
        }
    }
    Some(format!("{prefix}{}", parts.join("/")))
}

fn lookup(map: &DecodedMap, line: u32, column: u32) -> Option<OriginalPosition<'_>> {
    if let DecodedMap::Index(index) = map {
        // Metro may emit one section per module; select it in O(log sections).
        let mut low = 0;
        let mut high = index.get_section_count();
        while low < high {
            let mid = low + (high - low) / 2;
            if index.get_section(mid)?.get_offset() <= (line, column) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let section = index.get_section(low.checked_sub(1)?)?;
        let local_line = line.checked_sub(section.get_offset_line())?;
        let local_column = if local_line == 0 {
            column.checked_sub(section.get_offset_col())?
        } else {
            column
        };
        return lookup(section.get_sourcemap()?, local_line, local_column);
    }
    let token = map.lookup_token(line, column)?;
    // Greatest-lower-bound lookup must not borrow a mapping from a prior line.
    if token.get_dst_line() != line {
        return None;
    }
    let source = token.get_source()?;
    let name = match map {
        DecodedMap::Hermes(hermes) => hermes
            .get_scope_for_token(token)
            .or_else(|| token.get_name()),
        _ => token.get_name(),
    };
    Some(OriginalPosition {
        source,
        line: token.get_src_line().checked_add(1)?,
        column: token.get_src_col().checked_add(1)?,
        name: name.filter(|name| !name.is_empty()),
    })
}

/// Source maps keyed by generated file path, scoped to one build.
#[derive(Default)]
pub struct JavaScriptMapping {
    maps: HashMap<String, Arc<SourceMap>>,
}

impl JavaScriptMapping {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts owned or shared parsed maps, avoiding copies of cached map data.
    pub fn insert(
        &mut self,
        file_name: &str,
        map: impl Into<Arc<SourceMap>>,
    ) -> Option<Arc<SourceMap>> {
        self.maps
            .insert(normalize_file_name(file_name).to_owned(), map.into())
    }

    /// Generated file paths required by this trace. May contain duplicates.
    /// Callers can fetch these maps, insert them, then call `Mapping::apply`.
    pub fn files(stacktrace: &str) -> impl Iterator<Item = &str> {
        stacktrace
            .lines()
            .filter_map(|line| Frame::parse(line, false))
            .map(|frame| frame.file_name)
    }
}

impl Mapping for JavaScriptMapping {
    fn apply(&self, stacktrace: &str) -> Option<String> {
        apply_lines(stacktrace, false, |frame| {
            self.maps.get(frame.file_name).map(AsRef::as_ref)
        })
    }
}

/// A React Native bundle's Metro or composed Metro + Hermes source map.
/// File-less `name@1:offset` frames use this map too.
pub struct ReactNativeMapping {
    map: Arc<SourceMap>,
}

impl ReactNativeMapping {
    pub fn new(map: impl Into<Arc<SourceMap>>) -> Self {
        Self { map: map.into() }
    }
}

impl Mapping for ReactNativeMapping {
    fn apply(&self, stacktrace: &str) -> Option<String> {
        apply_lines(stacktrace, true, |_| Some(&self.map))
    }
}

fn apply_lines<'m>(
    stacktrace: &str,
    react_native: bool,
    map: impl Fn(&Frame<'_>) -> Option<&'m SourceMap>,
) -> Option<String> {
    let mut out = crate::Rewritten::new(stacktrace);
    let mut offset = 0;
    for line in stacktrace.split_inclusive('\n') {
        if let Some(frame) = Frame::parse(line, react_native)
            && let Some(map) = map(&frame)
            && let Some(position) = lookup(&map.0, frame.line, frame.column)
        {
            frame.write(position, out.replace(offset..offset + line.len()));
        }
        offset += line.len();
    }
    out.finish()
}

struct Frame<'a> {
    prefix: &'a str,
    body: &'a str,
    file_name: &'a str,
    line: u32,
    column: u32,
    tail: &'a str,
}

struct OriginalPosition<'a> {
    source: &'a str,
    line: u32,
    column: u32,
    name: Option<&'a str>,
}

impl<'a> Frame<'a> {
    fn parse(line: &'a str, react_native: bool) -> Option<Self> {
        let trimmed = line.trim_end();
        if !trimmed
            .as_bytes()
            .last()
            .is_some_and(|b| b.is_ascii_digit() || *b == b')')
        {
            return None;
        }
        let location = trimmed.strip_suffix(')').unwrap_or(trimmed);
        let (before_column, column) = location.rsplit_once(':')?;
        let column: u32 = column.parse().ok()?;
        let (file_part, line_no, fileless) = match before_column.rsplit_once(':') {
            Some((file, number))
                if number.bytes().all(|b| b.is_ascii_digit()) && !number.is_empty() =>
            {
                (file, number.parse::<u32>().ok()?, false)
            }
            _ if react_native => {
                let (function, number) = before_column.rsplit_once('@')?;
                (function, number.parse::<u32>().ok()?, true)
            }
            _ => return None,
        };
        let bytecode = fileless || location.contains("address at ");
        let file_start = if fileless {
            file_part.len() + 1
        } else {
            file_part.rfind([' ', '(', '@']).map_or(0, |idx| idx + 1)
        };
        let raw_file = if fileless {
            ""
        } else {
            &file_part[file_start..]
        };
        if !fileless && raw_file.is_empty() {
            return None;
        }
        let prefix = &trimmed[..file_start];
        Some(Self {
            prefix,
            body: trimmed,
            file_name: normalize_file_name(raw_file),
            line: line_no.checked_sub(1)?,
            column: if bytecode {
                column
            } else {
                column.checked_sub(1)?
            },
            tail: &line[location.len()..],
        })
    }

    fn function_prefix(&self) -> &str {
        let indent = self.body.len() - self.body.trim_start().len();
        let start = if self.prefix.ends_with('@') {
            // Preserve a logcat prefix when present.
            self.prefix.rfind(": ").map_or(indent, |idx| idx + 2)
        } else {
            indent
                + ["at async ", "at new ", "at "]
                    .iter()
                    .find(|prefix| self.body[indent..].starts_with(**prefix))
                    .map_or(0, |prefix| prefix.len())
        };
        &self.body[..start]
    }

    fn write(&self, position: OriginalPosition<'_>, out: &mut String) {
        let OriginalPosition {
            source,
            line,
            column,
            name,
        } = position;
        let trailing = self.tail.strip_prefix(')').unwrap_or(self.tail);
        match name {
            Some(name) if self.prefix.ends_with('@') => {
                let prefix = self.function_prefix();
                write!(out, "{prefix}{name}@{source}:{line}:{column}{trailing}").unwrap();
            }
            Some(name) => {
                let prefix = self.function_prefix();
                write!(out, "{prefix}{name} ({source}:{line}:{column}){trailing}").unwrap();
            }
            None => {
                let prefix = self
                    .prefix
                    .strip_suffix("address at ")
                    .unwrap_or(self.prefix);
                write!(out, "{prefix}{source}:{line}:{column}{}", self.tail).unwrap();
            }
        }
    }
}

/// Strips URL origin, query, fragment, and leading slashes for map selection.
fn normalize_file_name(raw_file: &str) -> &str {
    let path = raw_file.split(['?', '#']).next().unwrap_or(raw_file);
    let path = path
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or(path);
    path.trim_start_matches('/')
}
