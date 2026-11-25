//! Small helpers that keep the port's behaviour identical to the JavaScript it replaces
//! (String(x), Number(x), truthiness, string lengths in UTF-16 units, ISO timestamps).

use serde_json::Value;

/// JavaScript truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn number_string(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{}", f as i128)
    } else {
        format!("{f}")
    }
}

/// String(x)
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_i64().map(|i| i.to_string()).unwrap_or_else(|| number_string(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// Number(x)
pub fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                return 0.0;
            }
            match t {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ if t.starts_with("0x") || t.starts_with("0X") => i64::from_str_radix(&t[2..], 16).map(|i| i as f64).unwrap_or(f64::NAN),
                // Rust accepts "inf"/"nan" spellings that JavaScript doesn't.
                _ if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') => f64::NAN,
                _ => t.parse::<f64>().unwrap_or(f64::NAN),
            }
        }
        Value::Array(a) => match a.len() {
            0 => 0.0,
            1 => js_number(&Value::String(js_string(&a[0]))),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// A number as JSON the way JSON.stringify writes it: whole numbers without ".0", NaN/Infinity as null.
pub fn num(f: f64) -> Value {
    if !f.is_finite() {
        Value::Null
    } else if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

/// Number.isInteger for a JSON value.
pub fn is_int(v: &Value) -> bool {
    v.as_f64().is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

/// a === b for JSON values (numbers compare by value: 2 and 2.0 are the same).
pub fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// str.length
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// str.slice(0, n), never splitting a character.
pub fn utf16_take(s: &str, n: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in s.chars() {
        used += c.len_utf16();
        if used > n {
            break;
        }
        out.push(c);
    }
    out
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// new Date().toISOString()
pub fn iso_now() -> String {
    iso(now_ms())
}

pub fn iso(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conversions_match_javascript() {
        assert_eq!(js_string(&json!(null)), "null");
        assert_eq!(js_string(&json!(5)), "5");
        assert_eq!(js_string(&json!(2.5)), "2.5");
        assert_eq!(js_string(&json!(["a", null, 1])), "a,,1");
        assert_eq!(js_number(&json!("400")), 400.0);
        assert_eq!(js_number(&json!(" 12 ")), 12.0);
        assert_eq!(js_number(&json!("")), 0.0);
        assert!(js_number(&json!("abc")).is_nan());
        assert!(js_number(&json!("inf")).is_nan());
        assert_eq!(js_number(&json!(true)), 1.0);
        assert_eq!(num(400.0), json!(400));
        assert_eq!(num(f64::NAN), json!(null));
        assert!(strict_eq(&json!(2), &json!(2.0)));
        assert!(!truthy(&json!("")) && truthy(&json!("0")) && !truthy(&json!(0)));
        assert_eq!(utf16_take("ab😀c", 3), "ab");
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_795_565_844_123), "2026-11-25T00:17:24.123Z");
    }
}
