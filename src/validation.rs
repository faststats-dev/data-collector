use crate::models::DataSource;
use moka::sync::Cache;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tracing::debug;

static REGEX_CACHE: LazyLock<Cache<Arc<str>, Arc<Regex>>> =
    LazyLock::new(|| Cache::builder().max_capacity(100).build());

fn get_cached_regex(pattern: &str) -> Option<Arc<Regex>> {
    let key: Arc<str> = pattern.into();
    if let Some(re) = REGEX_CACHE.get(&key) {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => {
            let arc_re = Arc::new(re);
            REGEX_CACHE.insert(key, Arc::clone(&arc_re));
            Some(arc_re)
        }
        Err(_) => None,
    }
}

pub fn validate_and_filter_payload(
    mut data: HashMap<String, Value>,
    ds_by_ref: &HashMap<String, DataSource>,
) -> (HashMap<String, Value>, HashMap<String, String>) {
    let mut warnings = HashMap::new();

    data.retain(|ref_id, value| {
        let Some(ds) = ds_by_ref.get(ref_id) else {
            warnings.insert(ref_id.clone(), "no matching data source".into());
            debug!(
                "key='{}' VALID=false reason='no matching data source'",
                ref_id
            );
            return false;
        };

        let re = ds.regex.as_deref().and_then(get_cached_regex);

        if ds.is_array {
            if let Some(arr) = value.as_array_mut() {
                let mut has_invalid = false;
                let mut first_invalid_idx = None;
                let mut idx = 0;

                arr.retain(|elem| {
                    let is_valid = validate_scalar(elem, ds, re.as_deref()).is_ok();
                    if !is_valid {
                        if !has_invalid {
                            first_invalid_idx = Some(idx);
                            has_invalid = true;
                        }
                        debug!("key='{}' element[{}] VALID=false", ref_id, idx);
                    }
                    idx += 1;
                    is_valid
                });

                if has_invalid {
                    warnings.insert(
                        ref_id.clone(),
                        format!(
                            "element[{}] failed validation",
                            first_invalid_idx.unwrap_or(0)
                        ),
                    );
                }

                debug!(
                    "key='{}' VALID=true ({} elements remain)",
                    ref_id,
                    arr.len()
                );
                true
            } else {
                warnings.insert(ref_id.clone(), "expected array".into());
                debug!("key='{}' VALID=false reason='expected array'", ref_id);
                false
            }
        } else {
            if value.is_array() {
                warnings.insert(
                    ref_id.clone(),
                    "unexpected array (expected single value)".into(),
                );
                debug!(
                    "key='{}' VALID=false reason='unexpected array (expected single value)'",
                    ref_id
                );
                return false;
            }

            match validate_scalar(value, ds, re.as_deref()) {
                Ok(_) => {
                    debug!("key='{}' VALID=true", ref_id);
                    true
                }
                Err(_) => {
                    warnings.insert(ref_id.clone(), "failed validation".into());
                    debug!("key='{}' VALID=false", ref_id);
                    false
                }
            }
        }
    });

    (data, warnings)
}

fn validate_scalar(v: &Value, ds: &DataSource, re: Option<&Regex>) -> Result<(), &'static str> {
    match ds.data_type.as_str() {
        "string" => {
            let s = v.as_str().ok_or("expected string")?;
            if let Some(re) = re
                && !re.is_match(s)
            {
                return Err("regex mismatch");
            }
            Ok(())
        }
        "boolean" => {
            if v.as_bool().is_none() {
                return Err("expected boolean");
            }
            Ok(())
        }
        "number" => {
            let n = extract_number(v).ok_or("expected number")?;
            if let Some(false) = ds.allow_float
                && n.fract() != 0.0
            {
                return Err("float not allowed");
            }
            if let Some(false) = ds.allow_negative
                && n < 0.0
            {
                return Err("negative not allowed");
            }
            if let Some(min) = ds.min_value
                && n < min
            {
                return Err("value below minimum");
            }
            if let Some(max) = ds.max_value
                && n > max
            {
                return Err("value above maximum");
            }
            if !n.is_finite() {
                return Err("NaN/Infinity not allowed");
            }
            Ok(())
        }
        _ => Err("unsupported data_type"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_data_source(data_type: &str) -> DataSource {
        DataSource {
            reference_id: "test".to_string(),
            name: "Test Field".to_string(),
            data_type: data_type.to_string(),
            regex: None,
            allow_negative: None,
            allow_float: None,
            min_value: None,
            max_value: None,
            is_array: false,
        }
    }

    // ==================== STRING VALIDATION TESTS ====================

    mod string_validation {
        use super::*;

        #[test]
        fn validates_simple_string() {
            let ds = make_data_source("string");
            let value = json!("hello world");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_number_as_string() {
            let ds = make_data_source("string");
            let value = json!(42);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_boolean_as_string() {
            let ds = make_data_source("string");
            let value = json!(true);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_null_as_string() {
            let ds = make_data_source("string");
            let value = json!(null);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_object_as_string() {
            let ds = make_data_source("string");
            let value = json!({"key": "value"});
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn validates_empty_string() {
            let ds = make_data_source("string");
            let value = json!("");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_string_with_unicode() {
            let ds = make_data_source("string");
            let value = json!("hello 世界 🌍");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_string_matching_regex() {
            let ds = make_data_source("string");
            let re = Regex::new(r"^[a-z]+$").unwrap();
            let value = json!("hello");
            assert!(validate_scalar(&value, &ds, Some(&re)).is_ok());
        }

        #[test]
        fn rejects_string_not_matching_regex() {
            let ds = make_data_source("string");
            let re = Regex::new(r"^[a-z]+$").unwrap();
            let value = json!("Hello123");
            assert!(validate_scalar(&value, &ds, Some(&re)).is_err());
        }

        #[test]
        fn validates_email_regex() {
            let ds = make_data_source("string");
            let re = Regex::new(r"^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$").unwrap();

            let valid_email = json!("test@example.com");
            assert!(validate_scalar(&valid_email, &ds, Some(&re)).is_ok());

            let invalid_email = json!("not-an-email");
            assert!(validate_scalar(&invalid_email, &ds, Some(&re)).is_err());
        }

        #[test]
        fn validates_uuid_regex() {
            let ds = make_data_source("string");
            let re = Regex::new(
                r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
            )
            .unwrap();

            let valid_uuid = json!("550e8400-e29b-41d4-a716-446655440000");
            assert!(validate_scalar(&valid_uuid, &ds, Some(&re)).is_ok());

            let invalid_uuid = json!("not-a-uuid");
            assert!(validate_scalar(&invalid_uuid, &ds, Some(&re)).is_err());
        }
    }

    // ==================== BOOLEAN VALIDATION TESTS ====================

    mod boolean_validation {
        use super::*;

        #[test]
        fn validates_true() {
            let ds = make_data_source("boolean");
            let value = json!(true);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_false() {
            let ds = make_data_source("boolean");
            let value = json!(false);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_string_as_boolean() {
            let ds = make_data_source("boolean");
            let value = json!("true");
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_number_as_boolean() {
            let ds = make_data_source("boolean");
            let value = json!(1);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_zero_as_boolean() {
            let ds = make_data_source("boolean");
            let value = json!(0);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_null_as_boolean() {
            let ds = make_data_source("boolean");
            let value = json!(null);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }
    }

    // ==================== NUMBER VALIDATION TESTS ====================

    mod number_validation {
        use super::*;

        #[test]
        fn validates_integer() {
            let ds = make_data_source("number");
            let value = json!(42);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_float() {
            let ds = make_data_source("number");
            let value = json!(3.14159);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_negative_number() {
            let ds = make_data_source("number");
            let value = json!(-42);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_zero() {
            let ds = make_data_source("number");
            let value = json!(0);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_string_encoded_number() {
            let ds = make_data_source("number");
            let value = json!("42.5");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_string_encoded_negative() {
            let ds = make_data_source("number");
            let value = json!("-123.456");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_non_numeric_string() {
            let ds = make_data_source("number");
            let value = json!("not a number");
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_boolean_as_number() {
            let ds = make_data_source("number");
            let value = json!(true);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_null_as_number() {
            let ds = make_data_source("number");
            let value = json!(null);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        // Float constraint tests
        #[test]
        fn allows_float_when_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_float = Some(true);
            let value = json!(3.14);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_float_when_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_float = Some(false);
            let value = json!(3.14);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn allows_integer_when_float_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_float = Some(false);
            let value = json!(42);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn allows_float_ending_in_zero_when_float_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_float = Some(false);
            // 42.0 has fract() == 0.0, so it should be allowed
            let value = json!(42.0);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        // Negative constraint tests
        #[test]
        fn allows_negative_when_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_negative = Some(true);
            let value = json!(-42);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_negative_when_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_negative = Some(false);
            let value = json!(-42);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn allows_positive_when_negative_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_negative = Some(false);
            let value = json!(42);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn allows_zero_when_negative_not_permitted() {
            let mut ds = make_data_source("number");
            ds.allow_negative = Some(false);
            let value = json!(0);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        // Min/max constraint tests
        #[test]
        fn validates_number_above_min() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(10.0);
            let value = json!(15);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_number_at_min() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(10.0);
            let value = json!(10);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_number_below_min() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(10.0);
            let value = json!(5);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn validates_number_below_max() {
            let mut ds = make_data_source("number");
            ds.max_value = Some(100.0);
            let value = json!(50);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_number_at_max() {
            let mut ds = make_data_source("number");
            ds.max_value = Some(100.0);
            let value = json!(100);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_number_above_max() {
            let mut ds = make_data_source("number");
            ds.max_value = Some(100.0);
            let value = json!(150);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn validates_number_within_range() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(0.0);
            ds.max_value = Some(100.0);
            let value = json!(50);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn rejects_number_outside_range_below() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(0.0);
            ds.max_value = Some(100.0);
            let value = json!(-10);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        #[test]
        fn rejects_number_outside_range_above() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(0.0);
            ds.max_value = Some(100.0);
            let value = json!(150);
            assert!(validate_scalar(&value, &ds, None).is_err());
        }

        // NaN and Infinity tests
        #[test]
        fn rejects_infinity() {
            let ds = make_data_source("number");
            // Note: JSON doesn't support Infinity directly, but we test via string parsing
            // "Infinity" parses to f64::INFINITY which is not finite
            let string_inf = json!("Infinity");
            let result = validate_scalar(&string_inf, &ds, None);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_negative_infinity() {
            let ds = make_data_source("number");
            let string_neg_inf = json!("-Infinity");
            let result = validate_scalar(&string_neg_inf, &ds, None);
            assert!(result.is_err());
        }

        #[test]
        fn rejects_nan() {
            let ds = make_data_source("number");
            let string_nan = json!("NaN");
            let result = validate_scalar(&string_nan, &ds, None);
            assert!(result.is_err());
        }

        // Combined constraints
        #[test]
        fn validates_with_multiple_constraints() {
            let mut ds = make_data_source("number");
            ds.allow_float = Some(false);
            ds.allow_negative = Some(false);
            ds.min_value = Some(1.0);
            ds.max_value = Some(100.0);

            let valid = json!(50);
            assert!(validate_scalar(&valid, &ds, None).is_ok());

            let negative = json!(-5);
            assert!(validate_scalar(&negative, &ds, None).is_err());

            let float = json!(50.5);
            assert!(validate_scalar(&float, &ds, None).is_err());

            let below_min = json!(0);
            assert!(validate_scalar(&below_min, &ds, None).is_err());

            let above_max = json!(101);
            assert!(validate_scalar(&above_max, &ds, None).is_err());
        }

        // Large numbers
        #[test]
        fn validates_large_number() {
            let ds = make_data_source("number");
            let value = json!(9007199254740991_i64); // MAX_SAFE_INTEGER in JS
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn validates_very_small_float() {
            let ds = make_data_source("number");
            let value = json!(0.000000001);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }
    }

    // ==================== UNSUPPORTED TYPE TESTS ====================

    mod unsupported_type_validation {
        use super::*;

        #[test]
        fn rejects_unsupported_data_type() {
            let ds = make_data_source("unknown_type");
            let value = json!("test");
            let result = validate_scalar(&value, &ds, None);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("unsupported data_type"));
        }

        #[test]
        fn rejects_empty_data_type() {
            let ds = make_data_source("");
            let value = json!("test");
            assert!(validate_scalar(&value, &ds, None).is_err());
        }
    }

    // ==================== ARRAY VALIDATION TESTS ====================

    mod array_validation {
        use super::*;

        fn make_array_data_source(data_type: &str) -> DataSource {
            DataSource {
                reference_id: "test_array".to_string(),
                name: "Test Array Field".to_string(),
                data_type: data_type.to_string(),
                regex: None,
                allow_negative: None,
                allow_float: None,
                min_value: None,
                max_value: None,
                is_array: true,
            }
        }

        #[test]
        fn validates_string_array() {
            let ds = make_array_data_source("string");
            let mut ds_map = HashMap::new();
            ds_map.insert("tags".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("tags".to_string(), json!(["rust", "programming", "test"]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("tags"));
            assert_eq!(valid["tags"].as_array().unwrap().len(), 3);
            assert!(!warnings.contains_key("tags"));
        }

        #[test]
        fn validates_number_array() {
            let ds = make_array_data_source("number");
            let mut ds_map = HashMap::new();
            ds_map.insert("scores".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("scores".to_string(), json!([1, 2, 3, 4, 5]));

            let (valid, _warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("scores"));
            assert_eq!(valid["scores"].as_array().unwrap().len(), 5);
        }

        #[test]
        fn validates_boolean_array() {
            let ds = make_array_data_source("boolean");
            let mut ds_map = HashMap::new();
            ds_map.insert("flags".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("flags".to_string(), json!([true, false, true]));

            let (valid, _warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("flags"));
            assert_eq!(valid["flags"].as_array().unwrap().len(), 3);
        }

        #[test]
        fn filters_invalid_array_elements() {
            let ds = make_array_data_source("number");
            let mut ds_map = HashMap::new();
            ds_map.insert("values".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("values".to_string(), json!([1, "invalid", 3, null, 5]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("values"));
            let arr = valid["values"].as_array().unwrap();
            assert_eq!(arr.len(), 3); // Only 1, 3, 5 are valid
            assert!(warnings.contains_key("values"));
        }

        #[test]
        fn handles_empty_array() {
            let ds = make_array_data_source("string");
            let mut ds_map = HashMap::new();
            ds_map.insert("items".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("items".to_string(), json!([]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("items"));
            assert_eq!(valid["items"].as_array().unwrap().len(), 0);
            assert!(!warnings.contains_key("items"));
        }

        #[test]
        fn rejects_non_array_when_array_expected() {
            let ds = make_array_data_source("string");
            let mut ds_map = HashMap::new();
            ds_map.insert("items".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("items".to_string(), json!("not an array"));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(!valid.contains_key("items"));
            assert!(warnings.contains_key("items"));
            assert!(warnings["items"].contains("expected array"));
        }

        #[test]
        fn rejects_array_when_scalar_expected() {
            let ds = make_data_source("string");
            let mut ds_map = HashMap::new();
            ds_map.insert("name".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("name".to_string(), json!(["array", "of", "strings"]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(!valid.contains_key("name"));
            assert!(warnings.contains_key("name"));
            assert!(warnings["name"].contains("unexpected array"));
        }

        #[test]
        fn validates_array_with_regex() {
            let mut ds = make_array_data_source("string");
            ds.regex = Some(r"^[a-z]+$".to_string());
            let mut ds_map = HashMap::new();
            ds_map.insert("lowercase_words".to_string(), ds);

            let mut data = HashMap::new();
            data.insert(
                "lowercase_words".to_string(),
                json!(["hello", "WORLD", "test", "123"]),
            );

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("lowercase_words"));
            let arr = valid["lowercase_words"].as_array().unwrap();
            assert_eq!(arr.len(), 2); // Only "hello" and "test" match
            assert!(warnings.contains_key("lowercase_words"));
        }

        #[test]
        fn validates_array_with_number_constraints() {
            let mut ds = make_array_data_source("number");
            ds.min_value = Some(0.0);
            ds.max_value = Some(100.0);
            let mut ds_map = HashMap::new();
            ds_map.insert("percentages".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("percentages".to_string(), json!([10, 50, 150, -5, 100]));

            let (valid, _warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("percentages"));
            let arr = valid["percentages"].as_array().unwrap();
            assert_eq!(arr.len(), 3); // Only 10, 50, 100 are valid
        }
    }

    // ==================== VALIDATE_AND_FILTER_PAYLOAD TESTS ====================

    mod validate_and_filter_payload_tests {
        use super::*;

        #[test]
        fn returns_empty_for_empty_input() {
            let data = HashMap::new();
            let ds_map = HashMap::new();

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.is_empty());
            assert!(warnings.is_empty());
        }

        #[test]
        fn warns_on_unknown_field() {
            let mut data = HashMap::new();
            data.insert("unknown_field".to_string(), json!("value"));
            let ds_map = HashMap::new();

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(!valid.contains_key("unknown_field"));
            assert!(warnings.contains_key("unknown_field"));
            assert!(warnings["unknown_field"].contains("no matching data source"));
        }

        #[test]
        fn validates_multiple_fields() {
            let mut ds_map = HashMap::new();
            ds_map.insert("name".to_string(), make_data_source("string"));
            ds_map.insert("age".to_string(), make_data_source("number"));
            ds_map.insert("active".to_string(), make_data_source("boolean"));

            let mut data = HashMap::new();
            data.insert("name".to_string(), json!("John"));
            data.insert("age".to_string(), json!(30));
            data.insert("active".to_string(), json!(true));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert_eq!(valid.len(), 3);
            assert!(valid.contains_key("name"));
            assert!(valid.contains_key("age"));
            assert!(valid.contains_key("active"));
            assert!(warnings.is_empty());
        }

        #[test]
        fn separates_valid_and_invalid_fields() {
            let mut ds_map = HashMap::new();
            ds_map.insert("valid_string".to_string(), make_data_source("string"));
            ds_map.insert("invalid_number".to_string(), make_data_source("number"));

            let mut data = HashMap::new();
            data.insert("valid_string".to_string(), json!("hello"));
            data.insert("invalid_number".to_string(), json!("not a number"));
            data.insert("unknown".to_string(), json!("value"));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert_eq!(valid.len(), 1);
            assert!(valid.contains_key("valid_string"));
            assert_eq!(warnings.len(), 2);
            assert!(warnings.contains_key("invalid_number"));
            assert!(warnings.contains_key("unknown"));
        }

        #[test]
        fn handles_mixed_array_and_scalar_fields() {
            let mut ds_map = HashMap::new();
            let scalar_ds = make_data_source("string");
            let mut array_ds = make_data_source("number");
            array_ds.is_array = true;
            ds_map.insert("title".to_string(), scalar_ds);
            ds_map.insert("scores".to_string(), array_ds);

            let mut data = HashMap::new();
            data.insert("title".to_string(), json!("Test"));
            data.insert("scores".to_string(), json!([1, 2, 3]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert_eq!(valid.len(), 2);
            assert!(warnings.is_empty());
        }

        #[test]
        fn handles_regex_compilation_failure_gracefully() {
            let mut ds = make_data_source("string");
            ds.regex = Some("[invalid(regex".to_string()); // Invalid regex pattern
            let mut ds_map = HashMap::new();
            ds_map.insert("field".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("field".to_string(), json!("test"));

            // Should not panic, regex just won't be applied
            let (valid, _warnings) = validate_and_filter_payload(data, &ds_map);

            // Field should still be valid since regex couldn't be compiled
            assert!(valid.contains_key("field"));
        }

        #[test]
        fn preserves_original_values() {
            let mut ds_map = HashMap::new();
            ds_map.insert("data".to_string(), make_data_source("string"));

            let original_value = json!("test value with special chars: <>&\"'");
            let mut data = HashMap::new();
            data.insert("data".to_string(), original_value.clone());

            let (valid, _) = validate_and_filter_payload(data, &ds_map);

            assert_eq!(valid["data"], original_value);
        }
    }

    // ==================== EXTRACT_NUMBER TESTS ====================

    mod extract_number_tests {
        use super::*;

        #[test]
        fn extracts_integer() {
            let value = json!(42);
            assert_eq!(extract_number(&value), Some(42.0));
        }

        #[test]
        fn extracts_float() {
            let value = json!(3.14);
            assert_eq!(extract_number(&value), Some(3.14));
        }

        #[test]
        fn extracts_negative() {
            let value = json!(-100);
            assert_eq!(extract_number(&value), Some(-100.0));
        }

        #[test]
        fn extracts_from_string() {
            let value = json!("42.5");
            assert_eq!(extract_number(&value), Some(42.5));
        }

        #[test]
        fn extracts_negative_from_string() {
            let value = json!("-123");
            assert_eq!(extract_number(&value), Some(-123.0));
        }

        #[test]
        fn returns_none_for_invalid_string() {
            let value = json!("not a number");
            assert_eq!(extract_number(&value), None);
        }

        #[test]
        fn returns_none_for_boolean() {
            let value = json!(true);
            assert_eq!(extract_number(&value), None);
        }

        #[test]
        fn returns_none_for_null() {
            let value = json!(null);
            assert_eq!(extract_number(&value), None);
        }

        #[test]
        fn returns_none_for_object() {
            let value = json!({"num": 42});
            assert_eq!(extract_number(&value), None);
        }

        #[test]
        fn returns_none_for_array() {
            let value = json!([1, 2, 3]);
            assert_eq!(extract_number(&value), None);
        }

        #[test]
        fn extracts_scientific_notation() {
            let value = json!(1.5e10);
            let result = extract_number(&value);
            assert!(result.is_some());
            assert!((result.unwrap() - 1.5e10).abs() < 1.0);
        }

        #[test]
        fn extracts_scientific_notation_from_string() {
            let value = json!("1.5e10");
            let result = extract_number(&value);
            assert!(result.is_some());
            assert!((result.unwrap() - 1.5e10).abs() < 1.0);
        }
    }

    // ==================== EDGE CASE TESTS ====================

    mod edge_cases {
        use super::*;

        #[test]
        fn handles_very_long_string() {
            let ds = make_data_source("string");
            let long_string = "a".repeat(100_000);
            let value = json!(long_string);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn handles_deeply_nested_json_in_string() {
            let ds = make_data_source("string");
            let nested_json = r#"{"a":{"b":{"c":{"d":"value"}}}}"#;
            let value = json!(nested_json);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn handles_whitespace_only_string() {
            let ds = make_data_source("string");
            let value = json!("   \t\n  ");
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn handles_special_float_values() {
            let ds = make_data_source("number");

            // Negative zero
            let neg_zero = json!(-0.0);
            assert!(validate_scalar(&neg_zero, &ds, None).is_ok());

            // Very small positive
            let tiny = json!(f64::MIN_POSITIVE);
            assert!(validate_scalar(&tiny, &ds, None).is_ok());
        }

        #[test]
        fn handles_number_at_float_precision_boundary() {
            let ds = make_data_source("number");
            // Large integer that might lose precision as float
            let value = json!(9007199254740993_i64);
            assert!(validate_scalar(&value, &ds, None).is_ok());
        }

        #[test]
        fn min_equals_max_constraint() {
            let mut ds = make_data_source("number");
            ds.min_value = Some(42.0);
            ds.max_value = Some(42.0);

            let exact = json!(42);
            assert!(validate_scalar(&exact, &ds, None).is_ok());

            let not_exact = json!(43);
            assert!(validate_scalar(&not_exact, &ds, None).is_err());
        }

        #[test]
        fn handles_null_in_array() {
            let mut ds = make_data_source("string");
            ds.is_array = true;
            let mut ds_map = HashMap::new();
            ds_map.insert("items".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("items".to_string(), json!(["valid", null, "also valid"]));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            let arr = valid["items"].as_array().unwrap();
            assert_eq!(arr.len(), 2); // null should be filtered out
            assert!(warnings.contains_key("items"));
        }

        #[test]
        fn handles_object_in_array() {
            let mut ds = make_data_source("string");
            ds.is_array = true;
            let mut ds_map = HashMap::new();
            ds_map.insert("items".to_string(), ds);

            let mut data = HashMap::new();
            data.insert(
                "items".to_string(),
                json!(["valid", {"obj": "value"}, "also valid"]),
            );

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            let arr = valid["items"].as_array().unwrap();
            assert_eq!(arr.len(), 2); // object should be filtered out
            assert!(warnings.contains_key("items"));
        }

        #[test]
        fn handles_empty_reference_id() {
            let mut ds = make_data_source("string");
            ds.reference_id = "".to_string();
            let mut ds_map = HashMap::new();
            ds_map.insert("".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("".to_string(), json!("value"));

            let (valid, _warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key(""));
        }

        #[test]
        fn handles_unicode_field_names() {
            let ds = make_data_source("string");
            let mut ds_map = HashMap::new();
            ds_map.insert("フィールド".to_string(), ds);

            let mut data = HashMap::new();
            data.insert("フィールド".to_string(), json!("値"));

            let (valid, warnings) = validate_and_filter_payload(data, &ds_map);

            assert!(valid.contains_key("フィールド"));
            assert!(warnings.is_empty());
        }
    }
}
