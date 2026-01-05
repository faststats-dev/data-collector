use crate::models::DataSource;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;

macro_rules! debug {
    ($($arg:tt)*) => {
        if cfg!(debug_assertions) {
            eprintln!("[validate_payload][debug] {}", format!($($arg)*));
        }
    }
}

pub fn validate_and_filter_payload(
    data: &HashMap<String, Value>,
    ds_by_ref: &HashMap<String, DataSource>,
) -> (HashMap<String, Value>, HashMap<String, String>) {
    let mut valid_data = HashMap::new();
    let mut warnings = HashMap::new();
    let mut re_cache: HashMap<String, Regex> = HashMap::new();

    for (ref_id, ds) in ds_by_ref {
        if let Some(pat) = &ds.regex {
            if let Ok(re) = Regex::new(pat) {
                re_cache.insert(ref_id.clone(), re);
            }
        }
    }

    for (ref_id, value) in data {
        let Some(ds) = ds_by_ref.get(ref_id) else {
            warnings.insert(ref_id.clone(), "no matching data source".to_string());
            debug!(
                "key='{}' VALID=false reason='no matching data source'",
                ref_id
            );
            continue;
        };

        if ds.is_array {
            let Some(arr) = value.as_array() else {
                warnings.insert(ref_id.clone(), "expected array".to_string());
                debug!("key='{}' VALID=false reason='expected array'", ref_id);
                continue;
            };

            let mut valid_elements = Vec::new();
            for (idx, elem) in arr.iter().enumerate() {
                if validate_scalar(elem, ds, re_cache.get(ref_id)).is_ok() {
                    valid_elements.push(elem.clone());
                } else {
                    warnings.insert(
                        ref_id.clone(),
                        format!("element[{}] failed validation", idx),
                    );
                    debug!("key='{}' element[{}] VALID=false", ref_id, idx);
                }
            }

            valid_data.insert(ref_id.clone(), Value::Array(valid_elements.clone()));
            let valid_count = valid_elements.len();
            debug!(
                "key='{}' VALID=true ({} valid elements)",
                ref_id, valid_count
            );
        } else {
            if value.is_array() {
                warnings.insert(
                    ref_id.clone(),
                    "unexpected array (expected single value)".to_string(),
                );
                debug!(
                    "key='{}' VALID=false reason='unexpected array (expected single value)'",
                    ref_id
                );
                continue;
            }

            if validate_scalar(value, ds, re_cache.get(ref_id)).is_ok() {
                valid_data.insert(ref_id.clone(), value.clone());
                debug!("key='{}' VALID=true", ref_id);
            } else {
                warnings.insert(ref_id.clone(), "failed validation".to_string());
                debug!("key='{}' VALID=false", ref_id);
            }
        }
    }

    (valid_data, warnings)
}

fn validate_scalar(v: &Value, ds: &DataSource, re: Option<&Regex>) -> Result<(), String> {
    match ds.data_type.as_str() {
        "string" => {
            let s = v.as_str().ok_or_else(|| "expected string".to_string())?;
            if let Some(re) = re {
                if !re.is_match(s) {
                    return Err("regex mismatch".to_string());
                }
            }
            Ok(())
        }
        "boolean" => {
            if v.as_bool().is_none() {
                return Err("expected boolean".to_string());
            }
            Ok(())
        }
        "number" => {
            let n = extract_number(v).ok_or_else(|| "expected number".to_string())?;
            if let Some(false) = ds.allow_float {
                if n.fract() != 0.0 {
                    return Err("float not allowed; expected integer".to_string());
                }
            }
            if let Some(false) = ds.allow_negative {
                if n < 0.0 {
                    return Err("negative not allowed".to_string());
                }
            }
            if let Some(min) = ds.min_value {
                if n < min {
                    return Err(format!("value {} < min {}", n, min));
                }
            }
            if let Some(max) = ds.max_value {
                if n > max {
                    return Err(format!("value {} > max {}", n, max));
                }
            }
            if !n.is_finite() {
                return Err("NaN/Infinity not allowed".to_string());
            }
            Ok(())
        }
        other => Err(format!("unsupported data_type: {}", other)),
    }
}

fn extract_number(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        Some(n)
    } else if let Some(s) = v.as_str() {
        s.parse::<f64>().ok()
    } else {
        None
    }
}
