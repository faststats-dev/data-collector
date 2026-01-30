use std::sync::OnceLock;
use ua_parser::Extractor;

static UA_EXTRACTOR: OnceLock<Extractor> = OnceLock::new();

pub struct UserAgentInfo {
    pub browser: String,
    pub os: String,
    pub device: &'static str,
}

pub fn init() -> Result<(), String> {
    let path = std::env::var("UA_REGEXES_PATH").unwrap_or_else(|_| "regexes.yaml".into());
    let file = std::fs::File::open(&path).map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let regexes: ua_parser::Regexes = serde_yaml::from_reader(file)
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

    Some(UserAgentInfo {
        browser: browser
            .map(|u| format_version(&u.family, u.major, u.minor))
            .unwrap_or_default(),
        os: os
            .map(|o| format_version(&o.os, o.major, o.minor))
            .unwrap_or_default(),
        device: device_type,
    })
}

fn format_version(
    name: &str,
    major: Option<impl AsRef<str>>,
    minor: Option<impl AsRef<str>>,
) -> String {
    match (
        major.as_ref().map(|s| s.as_ref()),
        minor.as_ref().map(|s| s.as_ref()),
    ) {
        (Some(maj), Some(min)) => format!("{} {}.{}", name, maj, min),
        (Some(maj), None) => format!("{} {}", name, maj),
        _ => name.to_string(),
    }
}

fn contains_any(ua: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| ua.contains(p))
}

fn get_device_type(ua: &str, device_family: Option<&str>) -> &'static str {
    let ua = ua.to_ascii_lowercase();

    let is_bot = device_family.is_some_and(|f| {
        f.eq_ignore_ascii_case("spider") || f.to_ascii_lowercase().contains("bot")
    });
    if is_bot || contains_any(&ua, &["bot", "spider", "crawler", "headless"]) {
        return "Bot";
    }

    let is_android = ua.contains("android");
    let is_tablet = ua.contains("tablet");
    let is_mobile = ua.contains("mobile");

    if is_mobile
        || contains_any(&ua, &["iphone", "ipod", "windows phone", "blackberry"])
        || (is_android && !is_tablet)
    {
        return "Mobile";
    }
    if is_tablet || ua.contains("ipad") || ua.contains("kindle") || (is_android && !is_mobile) {
        return "Tablet";
    }
    if contains_any(
        &ua,
        &["smart-tv", "smarttv", "googletv", "appletv", "hbbtv"],
    ) {
        return "TV";
    }
    if contains_any(&ua, &["playstation", "xbox", "nintendo"]) {
        return "Console";
    }
    if ua.contains("watch") {
        return "Wearable";
    }
    "Desktop"
}
