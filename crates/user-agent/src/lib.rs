use regex::{Captures, Regex, RegexSet, RegexSetBuilder};
use std::{borrow::Cow, sync::OnceLock};

mod rules {
    include!(concat!(env!("OUT_DIR"), "/rules.rs"));
}

#[cfg(test)]
#[path = "../build/regex.rs"]
mod regex_rewrite;
#[cfg(test)]
use regex_rewrite::rewrite_regex;

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
    rules: RuleMatcher,
}

impl Parser {
    fn compile() -> Result<Self, regex::Error> {
        Ok(Self {
            rules: RuleMatcher::compile()?,
        })
    }

    fn parse(&self, user_agent: &str) -> Option<UserAgentInfo> {
        let matches = self.rules.matches(user_agent);
        let device = classify_device(user_agent, matches.device_is_bot);
        if device == "Bot" {
            return None;
        }

        let (browser, browser_version) = matches
            .user_agent
            .and_then(|(rule, regex)| {
                let captures = regex.captures(user_agent)?;
                let family = resolve_family(&captures, rule.replacement);
                let major = resolve_optional(&captures, rule.v1_replacement, 2);
                let minor = resolve_optional(&captures, rule.v2_replacement, 3);
                Some((family, format_version(major, minor)))
            })
            .unwrap_or_default();

        let (os, os_version) = matches
            .operating_system
            .and_then(|(rule, regex)| {
                let captures = regex.captures(user_agent)?;
                let name = resolve(&captures, rule.replacement, 1);
                let major = resolve_optional(&captures, rule.v1_replacement, 2);
                let minor = resolve_optional(&captures, rule.v2_replacement, 3);
                Some((name, format_version(major, minor)))
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
    user_agents: Vec<Regex>,
    operating_systems: Vec<Regex>,
}

struct MetadataMatches<'a> {
    user_agent: Option<(&'static Rule, &'a Regex)>,
    operating_system: Option<(&'static Rule, &'a Regex)>,
    device_is_bot: bool,
}

impl RuleMatcher {
    fn compile() -> Result<Self, regex::Error> {
        let patterns = UA_RULES
            .iter()
            .chain(OS_RULES)
            .map(|rule| rule.regex)
            .chain(DEVICE_RULES.iter().map(|rule| rule.regex));
        let set = RegexSetBuilder::new(patterns)
            .size_limit(100 * 1024 * 1024)
            .dfa_size_limit(20 * 1024 * 1024)
            .build()?;
        let user_agents = UA_RULES
            .iter()
            .map(|rule| Regex::new(rule.regex))
            .collect::<Result<_, _>>()?;
        let operating_systems = OS_RULES
            .iter()
            .map(|rule| Regex::new(rule.regex))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            set,
            user_agents,
            operating_systems,
        })
    }

    fn matches(&self, user_agent: &str) -> MetadataMatches<'_> {
        let mut user_agent_match = None;
        let mut os_match = None;
        let mut device_is_bot = None;
        for index in self.set.matches(user_agent).iter() {
            if index < UA_RULES.len() {
                user_agent_match.get_or_insert((&UA_RULES[index], &self.user_agents[index]));
            } else if index < UA_RULES.len() + OS_RULES.len() {
                let index = index - UA_RULES.len();
                os_match.get_or_insert((&OS_RULES[index], &self.operating_systems[index]));
            } else if device_is_bot.is_none() {
                let index = index - UA_RULES.len() - OS_RULES.len();
                device_is_bot = Some(DEVICE_RULES[index].is_bot);
            }
            if user_agent_match.is_some() && os_match.is_some() && device_is_bot.is_some() {
                break;
            }
        }
        MetadataMatches {
            user_agent: user_agent_match,
            operating_system: os_match,
            device_is_bot: device_is_bot.unwrap_or(false),
        }
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

fn resolve_optional<'a>(
    captures: &Captures<'a>,
    replacement: Option<&'a str>,
    fallback: usize,
) -> Option<Cow<'a, str>> {
    match replacement.filter(|replacement| !replacement.trim().is_empty()) {
        Some(template) if has_substitution(template) => {
            let value = expand(captures, template);
            (!value.is_empty()).then_some(Cow::Owned(value))
        }
        Some(replacement) => Some(Cow::Borrowed(replacement)),
        None => capture(captures, fallback).map(Cow::Borrowed),
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

fn format_version(major: Option<Cow<'_, str>>, minor: Option<Cow<'_, str>>) -> String {
    match (major, minor) {
        (Some(major), Some(minor)) => {
            let mut version = String::with_capacity(major.len() + minor.len() + 1);
            version.push_str(&major);
            version.push('.');
            version.push_str(&minor);
            version
        }
        (Some(major), None) => major.into_owned(),
        _ => String::new(),
    }
}

fn classify_device(user_agent: &str, device_is_bot: bool) -> &'static str {
    if device_is_bot || contains_any_ci(user_agent, &["bot", "spider", "crawler", "headless"]) {
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
