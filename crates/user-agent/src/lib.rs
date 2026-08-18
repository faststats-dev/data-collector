use regex::{Captures, Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use std::sync::OnceLock;

mod rules {
    include!(concat!(env!("OUT_DIR"), "/rules.rs"));
}

use rules::{DEVICE_RULES, OS_RULES, Rule, UA_RULES};

static PARSER: OnceLock<Parser> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserAgentInfo {
    pub browser: String,
    pub browser_version: String,
    pub os: String,
    pub os_version: String,
    pub device: &'static str,
}

/// Compiles the generated rule set. Calling this during startup avoids doing the
/// one-time work on the first request.
pub fn init() {
    parser();
}

/// Parses a user-agent using the complete generated uap-core rule set.
///
/// Returns `None` when the user-agent belongs to a bot or crawler.
pub fn parse(user_agent: &str) -> Option<UserAgentInfo> {
    parser().parse(user_agent)
}

fn parser() -> &'static Parser {
    PARSER.get_or_init(|| Parser::compile().expect("generated uap-core rules must be valid"))
}

struct Parser {
    user_agents: RuleMatcher,
    operating_systems: RuleMatcher,
    devices: RuleMatcher,
}

impl Parser {
    fn compile() -> Result<Self, regex::Error> {
        Ok(Self {
            user_agents: RuleMatcher::compile(UA_RULES)?,
            operating_systems: RuleMatcher::compile(OS_RULES)?,
            devices: RuleMatcher::compile(DEVICE_RULES)?,
        })
    }

    fn parse(&self, user_agent: &str) -> Option<UserAgentInfo> {
        let device_family = self
            .devices
            .captures(user_agent)
            .map(|(rule, captures)| resolve(&captures, rule.replacement, 1));
        let device = classify_device(user_agent, device_family.as_deref());
        if device == "Bot" {
            return None;
        }

        let (browser, browser_version) = self
            .user_agents
            .captures(user_agent)
            .map(|(rule, captures)| {
                let family = resolve_family(&captures, rule.replacement);
                let major = resolve_optional(&captures, rule.v1_replacement, 2);
                let minor = resolve_optional(&captures, rule.v2_replacement, 3);
                (family, format_version(major.as_deref(), minor.as_deref()))
            })
            .unwrap_or_default();

        let (os, os_version) = self
            .operating_systems
            .captures(user_agent)
            .map(|(rule, captures)| {
                let name = resolve(&captures, rule.replacement, 1);
                let major = resolve_optional(&captures, rule.v1_replacement, 2);
                let minor = resolve_optional(&captures, rule.v2_replacement, 3);
                (name, format_version(major.as_deref(), minor.as_deref()))
            })
            .unwrap_or_default();

        Some(UserAgentInfo {
            browser,
            browser_version,
            os,
            os_version,
            device,
        })
    }
}

struct RuleMatcher {
    set: RegexSet,
    rules: Vec<(Regex, &'static Rule)>,
}

impl RuleMatcher {
    fn compile(rules: &'static [Rule]) -> Result<Self, regex::Error> {
        let rewritten: Vec<(String, bool)> = rules
            .iter()
            .map(|rule| (rewrite_regex(rule.regex), rule.ignore_case))
            .collect();
        let set_patterns: Vec<String> = rewritten
            .iter()
            .map(|(regex, ignore_case)| {
                if *ignore_case {
                    format!("(?i:{regex})")
                } else {
                    regex.clone()
                }
            })
            .collect();

        let set = RegexSetBuilder::new(set_patterns)
            .size_limit(100 * 1024 * 1024)
            .dfa_size_limit(20 * 1024 * 1024)
            .build()?;
        let rules = rewritten
            .into_iter()
            .zip(rules)
            .map(|((regex, ignore_case), rule)| {
                RegexBuilder::new(&regex)
                    .case_insensitive(ignore_case)
                    .build()
                    .map(|regex| (regex, rule))
            })
            .collect::<Result<_, _>>()?;

        Ok(Self { set, rules })
    }

    fn captures<'ua>(&self, user_agent: &'ua str) -> Option<(&'static Rule, Captures<'ua>)> {
        let index = self.set.matches(user_agent).iter().next()?;
        let (regex, rule) = &self.rules[index];
        Some((*rule, regex.captures(user_agent)?))
    }
}

fn resolve_family(captures: &Captures<'_>, replacement: Option<&str>) -> String {
    match replacement {
        Some(template) if template.contains("$1") => {
            template.replace("$1", capture(captures, 1).unwrap_or_default())
        }
        Some(replacement) => replacement.to_owned(),
        None => capture(captures, 1).unwrap_or_default().to_owned(),
    }
}

fn resolve(captures: &Captures<'_>, replacement: Option<&str>, fallback: usize) -> String {
    match replacement.filter(|replacement| !replacement.trim().is_empty()) {
        Some(template) if has_substitution(template) => expand(captures, template),
        Some(replacement) => replacement.to_owned(),
        None => capture(captures, fallback).unwrap_or_default().to_owned(),
    }
}

fn resolve_optional(
    captures: &Captures<'_>,
    replacement: Option<&str>,
    fallback: usize,
) -> Option<String> {
    match replacement.filter(|replacement| !replacement.trim().is_empty()) {
        Some(template) if has_substitution(template) => {
            let value = expand(captures, template);
            (!value.is_empty()).then_some(value)
        }
        Some(replacement) => Some(replacement.to_owned()),
        None => capture(captures, fallback).map(str::to_owned),
    }
}

fn capture<'a>(captures: &Captures<'a>, index: usize) -> Option<&'a str> {
    captures
        .get(index)
        .map(|capture| capture.as_str())
        .filter(|capture| !capture.is_empty())
}

fn expand(captures: &Captures<'_>, template: &str) -> String {
    let mut value = String::new();
    captures.expand(template, &mut value);
    value.trim().to_owned()
}

fn has_substitution(template: &str) -> bool {
    template
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'$' && pair[1].is_ascii_digit())
}

fn format_version(major: Option<&str>, minor: Option<&str>) -> String {
    match (major, minor) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        (Some(major), None) => major.to_owned(),
        _ => String::new(),
    }
}

fn classify_device(user_agent: &str, device_family: Option<&str>) -> &'static str {
    if device_family
        .is_some_and(|family| family.eq_ignore_ascii_case("spider") || contains_ci(family, "bot"))
        || contains_any_ci(user_agent, &["bot", "spider", "crawler", "headless"])
    {
        return "Bot";
    }

    let android = contains_ci(user_agent, "android");
    let tablet = contains_ci(user_agent, "tablet");
    let mobile = contains_ci(user_agent, "mobile");

    if mobile
        || contains_any_ci(
            user_agent,
            &["iphone", "ipod", "windows phone", "blackberry"],
        )
        || (android && !tablet)
    {
        "Mobile"
    } else if tablet || contains_any_ci(user_agent, &["ipad", "kindle"]) || (android && !mobile) {
        "Tablet"
    } else if contains_any_ci(
        user_agent,
        &["smart-tv", "smarttv", "googletv", "appletv", "hbbtv"],
    ) {
        "TV"
    } else if contains_any_ci(user_agent, &["playstation", "xbox", "nintendo"]) {
        "Console"
    } else if contains_ci(user_agent, "watch") {
        "Wearable"
    } else {
        "Desktop"
    }
}

fn contains_any_ci(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| contains_ci(haystack, needle))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn rewrite_regex(regex: &str) -> String {
    let mut rewritten = String::with_capacity(regex.len());
    let mut characters = regex.chars().peekable();
    let mut in_class = false;

    while let Some(character) = characters.next() {
        match character {
            '[' => {
                in_class = true;
                rewritten.push(character);
            }
            ']' => {
                in_class = false;
                rewritten.push(character);
            }
            '\\' => match characters.next() {
                Some('d') => rewritten.push_str("[0-9]"),
                Some('D') => rewritten.push_str("[^0-9]"),
                Some('w') => rewritten.push_str("[A-Za-z0-9_]"),
                Some('W') => rewritten.push_str("[^A-Za-z0-9_]"),
                Some(next) => {
                    rewritten.push('\\');
                    rewritten.push(next);
                }
                None => rewritten.push('\\'),
            },
            '{' if !in_class => {
                let mut repetition = String::from("{");
                while let Some(&next) = characters.peek() {
                    repetition.push(next);
                    characters.next();
                    if next == '}' {
                        break;
                    }
                }
                if let Some(replacement) = unbounded_repetition(&repetition) {
                    rewritten.push_str(replacement);
                } else {
                    rewritten.push_str(&repetition);
                }
            }
            _ => rewritten.push(character),
        }
    }

    rewritten
}

fn unbounded_repetition(repetition: &str) -> Option<&'static str> {
    let inner = repetition.strip_prefix('{')?.strip_suffix('}')?;
    let (minimum, maximum) = inner.split_once(',')?;
    if maximum.len() <= 2 || !maximum.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match minimum {
        "0" => Some("*"),
        "1" => Some("+"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_user_agents_from_generated_rules() {
        let chrome = parse("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36").unwrap();
        assert_eq!(
            (chrome.browser.as_str(), chrome.browser_version.as_str()),
            ("Chrome", "126.0")
        );
        assert_eq!(
            (chrome.os.as_str(), chrome.os_version.as_str()),
            ("Windows", "10")
        );
        assert_eq!(chrome.device, "Desktop");

        let safari = parse("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1").unwrap();
        assert_eq!(
            (safari.browser.as_str(), safari.browser_version.as_str()),
            ("Mobile Safari", "17.5")
        );
        assert_eq!(
            (safari.os.as_str(), safari.os_version.as_str()),
            ("iOS", "17.5")
        );
        assert_eq!(safari.device, "Mobile");
    }

    #[test]
    fn applies_ordered_special_case_rules() {
        let edge = parse("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36 Edg/126.0.0.0").unwrap();
        let samsung = parse("Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 Chrome/121.0 Mobile Safari/537.36 SamsungBrowser/25.0").unwrap();

        assert_eq!(edge.browser, "Edge");
        assert_eq!(samsung.browser, "Samsung Internet");
    }

    #[test]
    fn filters_crawlers_recognized_by_uap_core() {
        assert!(
            parse("facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)")
                .is_none()
        );
        assert!(
            parse("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)")
                .is_none()
        );
    }

    #[test]
    fn rewrites_large_repetitions_and_ascii_classes() {
        assert_eq!(rewrite_regex(r"\dx"), "[0-9]x");
        assert_eq!(rewrite_regex("(.{0,100})"), "(.*)");
        assert_eq!(rewrite_regex("[^;]{1,200}"), "[^;]+");
        assert_eq!(rewrite_regex(".{0,2}"), ".{0,2}");
    }
}
