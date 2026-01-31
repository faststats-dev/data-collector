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

fn contains_any_ci(ua: &[u8], patterns: &[&[u8]]) -> bool {
    patterns
        .iter()
        .any(|p| ua.windows(p.len()).any(|w| w.eq_ignore_ascii_case(p)))
}

fn contains_ci(ua: &[u8], pattern: &[u8]) -> bool {
    ua.windows(pattern.len())
        .any(|w| w.eq_ignore_ascii_case(pattern))
}

fn get_device_type(ua: &str, device_family: Option<&str>) -> &'static str {
    let ua_bytes = ua.as_bytes();

    let is_bot = device_family
        .is_some_and(|f| f.eq_ignore_ascii_case("spider") || contains_ci(f.as_bytes(), b"bot"));
    if is_bot || contains_any_ci(ua_bytes, &[b"bot", b"spider", b"crawler", b"headless"]) {
        return "Bot";
    }

    let is_android = contains_ci(ua_bytes, b"android");
    let is_tablet = contains_ci(ua_bytes, b"tablet");
    let is_mobile = contains_ci(ua_bytes, b"mobile");

    if is_mobile
        || contains_any_ci(
            ua_bytes,
            &[b"iphone", b"ipod", b"windows phone", b"blackberry"],
        )
        || (is_android && !is_tablet)
    {
        return "Mobile";
    }
    if is_tablet
        || contains_ci(ua_bytes, b"ipad")
        || contains_ci(ua_bytes, b"kindle")
        || (is_android && !is_mobile)
    {
        return "Tablet";
    }
    if contains_any_ci(
        ua_bytes,
        &[b"smart-tv", b"smarttv", b"googletv", b"appletv", b"hbbtv"],
    ) {
        return "TV";
    }
    if contains_any_ci(ua_bytes, &[b"playstation", b"xbox", b"nintendo"]) {
        return "Console";
    }
    if contains_ci(ua_bytes, b"watch") {
        return "Wearable";
    }
    "Desktop"
}
