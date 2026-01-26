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
    let ua = ua.to_lowercase();

    if device_family
        .is_some_and(|f| f.eq_ignore_ascii_case("spider") || f.to_lowercase().contains("bot"))
        || ua.contains("bot")
        || ua.contains("spider")
        || ua.contains("crawler")
        || ua.contains("headless")
    {
        return "Bot";
    }
    if ua.contains("mobile")
        || ua.contains("iphone")
        || ua.contains("ipod")
        || (ua.contains("android") && !ua.contains("tablet"))
        || ua.contains("windows phone")
        || ua.contains("blackberry")
    {
        return "Mobile";
    }
    if ua.contains("tablet")
        || ua.contains("ipad")
        || ua.contains("kindle")
        || (ua.contains("android") && !ua.contains("mobile"))
    {
        return "Tablet";
    }
    if ua.contains("smart-tv")
        || ua.contains("smarttv")
        || ua.contains("googletv")
        || ua.contains("appletv")
        || ua.contains("hbbtv")
    {
        return "TV";
    }
    if ua.contains("playstation") || ua.contains("xbox") || ua.contains("nintendo") {
        return "Console";
    }
    if ua.contains("watch") {
        return "Wearable";
    }
    "Desktop"
}
