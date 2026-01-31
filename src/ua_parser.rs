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

fn get_device_type(ua: &str, device_family: Option<&str>) -> &'static str {
    let ua_lower = ua.to_lowercase();

    if let Some(f) = device_family {
        let f_lower = f.to_lowercase();
        if f_lower == "spider" || f_lower.contains("bot") {
            return "Bot";
        }
    }

    if ua_lower.contains("bot")
        || ua_lower.contains("spider")
        || ua_lower.contains("crawler")
        || ua_lower.contains("headless")
    {
        return "Bot";
    }

    let is_android = ua_lower.contains("android");
    let is_tablet = ua_lower.contains("tablet");
    let is_mobile = ua_lower.contains("mobile");

    if is_mobile
        || ua_lower.contains("iphone")
        || ua_lower.contains("ipod")
        || ua_lower.contains("windows phone")
        || ua_lower.contains("blackberry")
        || (is_android && !is_tablet)
    {
        return "Mobile";
    }

    if is_tablet
        || ua_lower.contains("ipad")
        || ua_lower.contains("kindle")
        || (is_android && !is_mobile)
    {
        return "Tablet";
    }

    if ua_lower.contains("smart-tv")
        || ua_lower.contains("smarttv")
        || ua_lower.contains("googletv")
        || ua_lower.contains("appletv")
        || ua_lower.contains("hbbtv")
    {
        return "TV";
    }

    if ua_lower.contains("playstation")
        || ua_lower.contains("xbox")
        || ua_lower.contains("nintendo")
    {
        return "Console";
    }

    if ua_lower.contains("watch") {
        return "Wearable";
    }

    "Desktop"
}
