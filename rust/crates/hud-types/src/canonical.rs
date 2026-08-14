//! Python-compatible `json.dumps(value, sort_keys=True)` for slug hashing.
//!
//! `TaskRow::default_slug` must produce the same SHA-1 as the Python SDK, which
//! hashes `json.dumps(args, sort_keys=True, default=str)` — default separators
//! (`", "` / `": "`) and `ensure_ascii=True`. This module reproduces that exact
//! byte stream for JSON values.

use serde_json::Value;
use std::fmt::Write;

/// Serialize like Python's `json.dumps(value, sort_keys=True)`.
pub fn python_json_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string(out, key);
                out.push_str(": ");
                write_value(out, &map[key]);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if n.is_i64() || n.is_u64() {
        write!(out, "{n}").expect("write to String");
        return;
    }
    let f = n.as_f64().unwrap_or(0.0);
    out.push_str(&python_float_repr(f));
}

/// Python `repr()` formatting for finite floats: shortest round-trip decimal,
/// fixed notation for exponents in [-4, 16), otherwise `e±NN` scientific with a
/// two-digit exponent and a mandatory sign.
fn python_float_repr(f: f64) -> String {
    // Rust's `{:?}` is the shortest round-trip decimal, like Python's repr, but
    // switches to `e`-notation at different thresholds and formats the exponent
    // differently. Normalize through digits + decimal exponent.
    let shortest = format!("{f:?}");
    let (mantissa, exp10) = match shortest.split_once(['e', 'E']) {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().unwrap_or(0)),
        None => (shortest, 0),
    };
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let point = mantissa
        .find('.')
        .map(|i| if negative { i - 1 } else { i })
        .unwrap_or(if negative {
            mantissa.len() - 1
        } else {
            mantissa.len()
        });
    let digits = digits.trim_start_matches('0');
    let leading_zeros = mantissa
        .trim_start_matches('-')
        .chars()
        .take_while(|c| *c == '0' || *c == '.')
        .filter(|c| *c == '0')
        .count();
    if digits.is_empty() {
        return if negative {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    // Decimal exponent of the leading significant digit.
    let e = point as i32 - leading_zeros as i32 - 1 + exp10;
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&e) {
        if e >= 0 {
            let e = e as usize;
            if digits.len() > e + 1 {
                format!("{sign}{}.{}", &digits[..=e], &digits[e + 1..])
            } else {
                format!("{sign}{}{}.0", digits, "0".repeat(e + 1 - digits.len()))
            }
        } else {
            format!("{sign}0.{}{}", "0".repeat((-e - 1) as usize), digits)
        }
    } else {
        let frac = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let (esign, eabs) = if e < 0 { ("-", -e) } else { ("+", e) };
        format!("{sign}{}{frac}e{esign}{eabs:02}", &digits[..1])
    }
}

/// Python `json.dumps` string escaping with `ensure_ascii=True`.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32).expect("write to String");
            }
            c if c.is_ascii() => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    write!(out, "\\u{unit:04x}").expect("write to String");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_dumps() {
        // Expected strings produced by:
        //   python3 -c 'import json; print(json.dumps(..., sort_keys=True))'
        assert_eq!(
            python_json_sorted(&json!({"b": 2, "a": {"y": [1, 2], "x": "s"}})),
            r#"{"a": {"x": "s", "y": [1, 2]}, "b": 2}"#
        );
        assert_eq!(
            python_json_sorted(&json!([true, false, null])),
            "[true, false, null]"
        );
        assert_eq!(
            python_json_sorted(&json!("h\u{e9}llo\n")),
            "\"h\\u00e9llo\\n\""
        );
        assert_eq!(
            python_json_sorted(&json!("\u{1F389}")),
            "\"\\ud83c\\udf89\""
        );
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(python_float_repr(1.0), "1.0");
        assert_eq!(python_float_repr(-1.0), "-1.0");
        assert_eq!(python_float_repr(0.5), "0.5");
        assert_eq!(python_float_repr(12.34), "12.34");
        assert_eq!(python_float_repr(0.0001), "0.0001");
        assert_eq!(python_float_repr(0.00001), "1e-05");
        assert_eq!(python_float_repr(1e16), "1e+16");
        assert_eq!(python_float_repr(1.5e16), "1.5e+16");
        assert_eq!(python_float_repr(1e15), "1000000000000000.0");
        assert_eq!(python_float_repr(0.0), "0.0");
        assert_eq!(python_float_repr(123.456), "123.456");
    }
}
