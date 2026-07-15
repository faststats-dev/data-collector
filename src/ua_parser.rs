use std::sync::OnceLock;
use ua_parser::Extractor;

static UA_EXTRACTOR: OnceLock<Extractor> = OnceLock::new();

pub struct UserAgentInfo {
    pub browser: String,
    pub browser_version: String,
    pub os: String,
    pub os_version: String,
    pub device: &'static str,
}

pub fn init() -> Result<(), String> {
    let path = std::env::var("UA_REGEXES_PATH").unwrap_or_else(|_| "regexes.yaml".into());
    let file = std::fs::File::open(&path).map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let regexes: ua_parser::Regexes = yaml_serde::from_reader(file)
        .map_err(|e| format!("Failed to parse regexes.yaml: {}", e))?;
    let extractor = Extractor::try_from(regexes)
        .map_err(|e| format!("Failed to create UA extractor: {}", e))?;
    UA_EXTRACTOR
        .set(extractor)
        .map_err(|_| "UA parser already initialized".to_string())
}

/// Returns None if bot detected, Some(info) otherwise
pub fn parse(ua: &str) -> Option<UserAgentInfo> {
    let extractor = UA_EXTRACTOR.get()?;
    let (browser, os, device) = extractor.extract(ua);

    let device_type = get_device_type(ua, device.as_ref().map(|d| d.device.as_ref()));
    if device_type == "Bot" {
        return None;
    }

    let (browser_name, browser_version) = browser
        .map(|u| {
            let version = format_version_only(u.major, u.minor);
            (u.family.to_string(), version)
        })
        .unwrap_or_default();

    let (os_name, os_version) = os
        .map(|o| {
            let version = format_version_only(o.major, o.minor);
            (o.os.to_string(), version)
        })
        .unwrap_or_default();

    Some(UserAgentInfo {
        browser: browser_name,
        browser_version,
        os: os_name,
        os_version,
        device: device_type,
    })
}

fn format_version_only(major: Option<impl AsRef<str>>, minor: Option<impl AsRef<str>>) -> String {
    match (
        major.as_ref().map(|s| s.as_ref()),
        minor.as_ref().map(|s| s.as_ref()),
    ) {
        (Some(maj), Some(min)) => format!("{}.{}", maj, min),
        (Some(maj), None) => maj.to_string(),
        _ => String::new(),
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn get_device_type(ua: &str, device_family: Option<&str>) -> &'static str {
    if let Some(f) = device_family
        && (f.eq_ignore_ascii_case("spider") || contains_ci(f, "bot"))
    {
        return "Bot";
    }

    if contains_ci(ua, "bot")
        || contains_ci(ua, "spider")
        || contains_ci(ua, "crawler")
        || contains_ci(ua, "headless")
    {
        return "Bot";
    }

    let is_android = contains_ci(ua, "android");
    let is_tablet = contains_ci(ua, "tablet");
    let is_mobile = contains_ci(ua, "mobile");

    if is_mobile
        || contains_ci(ua, "iphone")
        || contains_ci(ua, "ipod")
        || contains_ci(ua, "windows phone")
        || contains_ci(ua, "blackberry")
        || (is_android && !is_tablet)
    {
        return "Mobile";
    }

    if is_tablet
        || contains_ci(ua, "ipad")
        || contains_ci(ua, "kindle")
        || (is_android && !is_mobile)
    {
        return "Tablet";
    }

    if contains_ci(ua, "smart-tv")
        || contains_ci(ua, "smarttv")
        || contains_ci(ua, "googletv")
        || contains_ci(ua, "appletv")
        || contains_ci(ua, "hbbtv")
    {
        return "TV";
    }

    if contains_ci(ua, "playstation") || contains_ci(ua, "xbox") || contains_ci(ua, "nintendo") {
        return "Console";
    }

    if contains_ci(ua, "watch") {
        return "Wearable";
    }

    "Desktop"
}
