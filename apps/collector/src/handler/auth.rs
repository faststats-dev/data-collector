use super::{HandlerResponse, error_response};
use crate::models::DataSource;
use axum::http::{HeaderMap, StatusCode};
use moka::future::Cache;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use uuid::Uuid;

static PROJECT_CACHE: LazyLock<Cache<String, Arc<ProjectContext>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(1_000)
        .time_to_live(Duration::from_secs(60))
        .build()
});

pub fn get_authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")
        .and_then(|value| value.to_str().ok())
        .map(|auth| auth.strip_prefix("Bearer ").unwrap_or(auth).to_owned())
}

pub async fn authenticate_project(
    pool: &sqlx::PgPool,
    headers: &HeaderMap,
    body_token: Option<String>,
) -> Result<Arc<ProjectContext>, HandlerResponse> {
    let token = body_token
        .or_else(|| get_authorization(headers))
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Unauthorized"))?;
    load_project_context(pool, &token).await
}

pub struct IpRule {
    pub ip_address: String,
    pub allowed: bool,
}

pub struct ProjectContext {
    pub project_id: Uuid,
    pub replay_storage_generation: i32,
    pub allowed_hostnames: Vec<String>,
    pub datasources: HashMap<String, DataSource>,
    pub error_tracking_enabled: bool,
    pub web_vitals_enabled: bool,
    pub session_replays_enabled: bool,
    pub cookieless_mode: Option<bool>,
    pub ip_rules: Vec<IpRule>,
}

pub async fn load_project_context(
    pool: &sqlx::PgPool,
    token: &str,
) -> Result<Arc<ProjectContext>, HandlerResponse> {
    if let Some(cached) = PROJECT_CACHE.get(token).await {
        return Ok(cached);
    }

    let rows = sqlx::query(
        r#"
        SELECT p.id, p.allowed_hostnames, p.error_tracking_enabled,
               p.web_vitals_enabled, p.session_replays_enabled, p.cookieless_mode,
               p.replay_storage_generation,
               d.reference_id, d.data_type::text, d.regex, d.allow_negative,
               d.allow_float, d.min_value, d.max_value, d.metric_shape::text
        FROM project p
        LEFT JOIN data_sources d ON d.project_id = p.id
        WHERE p.token = $1
        "#,
    )
    .bind(token)
    .fetch_all(pool)
    .await
    .map_err(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "DB Error"))?;

    if rows.is_empty() {
        return Err(error_response(StatusCode::UNAUTHORIZED, "Unauthorized"));
    }

    let first = &rows[0];
    let mut datasources = HashMap::with_capacity(rows.len());

    for row in &rows {
        if let Ok(Some(ref_id)) = row.try_get::<Option<String>, _>("reference_id") {
            datasources.insert(
                ref_id,
                DataSource {
                    data_type: row.try_get::<String, _>("data_type").unwrap_or_default(),
                    regex: row
                        .try_get::<String, _>("regex")
                        .ok()
                        .and_then(|pattern| regex::Regex::new(&pattern).ok()),
                    allow_negative: row.try_get("allow_negative").ok(),
                    allow_float: row.try_get("allow_float").ok(),
                    min_value: row.try_get("min_value").ok(),
                    max_value: row.try_get("max_value").ok(),
                    metric_shape: row.try_get("metric_shape").ok(),
                },
            );
        }
    }

    let ip_rules =
        sqlx::query("SELECT ip_address, allowed FROM ip_addresses WHERE project_id = $1")
            .bind(first.get::<Uuid, _>("id"))
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| IpRule {
                ip_address: row.get("ip_address"),
                allowed: row.get("allowed"),
            })
            .collect();

    let project_id = first.get::<Uuid, _>("id");

    let ctx = Arc::new(ProjectContext {
        project_id,
        replay_storage_generation: first.get("replay_storage_generation"),
        allowed_hostnames: first
            .try_get::<sqlx::types::Json<Vec<String>>, _>("allowed_hostnames")
            .ok()
            .map(|j| j.0)
            .unwrap_or_default(),
        datasources,
        error_tracking_enabled: first.get("error_tracking_enabled"),
        web_vitals_enabled: first.get("web_vitals_enabled"),
        session_replays_enabled: first.get("session_replays_enabled"),
        cookieless_mode: first.get("cookieless_mode"),
        ip_rules,
    });
    PROJECT_CACHE
        .insert(token.to_string(), Arc::clone(&ctx))
        .await;
    Ok(ctx)
}

pub fn validate_hostname(allowed_hostnames: &[String], request_origin: Option<&str>) -> bool {
    if allowed_hostnames.is_empty() {
        return true;
    }
    let Some(origin) = request_origin else {
        return false;
    };
    let origin_lower = origin.to_ascii_lowercase();
    allowed_hostnames.iter().any(|pattern| {
        let p = pattern.to_ascii_lowercase();
        if p == "*" {
            true
        } else if let Some(suffix) = p.strip_prefix("*.") {
            origin_lower == suffix
                || origin_lower
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        } else {
            p == origin_lower
        }
    })
}

pub fn check_ip_allowed(ip_rules: &[IpRule], client_ip: &str) -> Result<(), &'static str> {
    if ip_rules.is_empty() {
        return Ok(());
    }

    let mut has_whitelist = false;
    let mut allowed_by_whitelist = false;

    for rule in ip_rules {
        if rule.allowed {
            has_whitelist = true;
            if rule.ip_address == client_ip {
                allowed_by_whitelist = true;
            }
        } else if rule.ip_address == client_ip {
            return Err("IP address blocked");
        }
    }

    if has_whitelist && !allowed_by_whitelist {
        return Err("IP address not allowed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod hostname_validation {
        use super::*;

        #[test]
        fn allows_all_when_no_hostnames_configured() {
            assert!(validate_hostname(&[], Some("example.com")));
            assert!(validate_hostname(&[], None));
        }

        #[test]
        fn rejects_when_hostnames_configured_but_no_origin() {
            assert!(!validate_hostname(&["example.com".into()], None));
        }

        #[test]
        fn allows_matching_hostname() {
            assert!(validate_hostname(
                &["example.com".into()],
                Some("example.com")
            ));
        }

        #[test]
        fn allows_matching_hostname_case_insensitive() {
            assert!(validate_hostname(
                &["Example.COM".into()],
                Some("example.com")
            ));
            assert!(validate_hostname(
                &["example.com".into()],
                Some("EXAMPLE.COM")
            ));
        }

        #[test]
        fn rejects_non_matching_hostname() {
            assert!(!validate_hostname(
                &["example.com".into()],
                Some("other.com")
            ));
        }

        #[test]
        fn allows_any_from_multiple_hostnames() {
            let hostnames: Vec<String> = vec!["example.com".into(), "other.com".into()];
            assert!(validate_hostname(&hostnames, Some("example.com")));
            assert!(validate_hostname(&hostnames, Some("other.com")));
            assert!(!validate_hostname(&hostnames, Some("nope.com")));
        }

        #[test]
        fn wildcard_matches_subdomains() {
            let hostnames: Vec<String> = vec!["*.example.com".into()];
            assert!(validate_hostname(&hostnames, Some("sub.example.com")));
            assert!(validate_hostname(&hostnames, Some("deep.sub.example.com")));
            assert!(validate_hostname(&hostnames, Some("example.com")));
            assert!(!validate_hostname(&hostnames, Some("other.com")));
        }

        #[test]
        fn star_allows_everything() {
            let hostnames: Vec<String> = vec!["*".into()];
            assert!(validate_hostname(&hostnames, Some("anything.com")));
            assert!(validate_hostname(&hostnames, Some("example.com")));
        }
    }

    mod ip_filtering {
        use super::*;

        #[test]
        fn allows_all_when_no_rules() {
            let rules: Vec<IpRule> = vec![];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_ok());
        }

        #[test]
        fn whitelist_allows_matching_ip() {
            let rules = vec![
                IpRule {
                    ip_address: "192.168.1.1".to_string(),
                    allowed: true,
                },
                IpRule {
                    ip_address: "192.168.1.2".to_string(),
                    allowed: true,
                },
            ];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_ok());
        }

        #[test]
        fn whitelist_blocks_non_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: true,
            }];
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_err());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_err());
        }

        #[test]
        fn blacklist_blocks_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: false,
            }];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_err());
        }

        #[test]
        fn blacklist_allows_non_matching_ip() {
            let rules = vec![IpRule {
                ip_address: "192.168.1.1".to_string(),
                allowed: false,
            }];
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_ok());
            assert!(check_ip_allowed(&rules, "192.168.1.2").is_ok());
        }

        #[test]
        fn whitelist_takes_precedence_over_blacklist() {
            let rules = vec![
                IpRule {
                    ip_address: "192.168.1.1".to_string(),
                    allowed: true,
                },
                IpRule {
                    ip_address: "10.0.0.1".to_string(),
                    allowed: false,
                },
            ];
            assert!(check_ip_allowed(&rules, "192.168.1.1").is_ok());
            assert!(check_ip_allowed(&rules, "10.0.0.1").is_err());
            assert!(check_ip_allowed(&rules, "172.16.0.1").is_err());
        }
    }
}
