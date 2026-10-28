//! Tool-argument validation with lenient coercion for agents.
//!
//! Driven by the tool's own `inputSchema`, it coerces leniently
//! (`"2"` → 2, `2.0` → 2, `"yes"` → true, `"62.5"` → 62.5), fills defaults,
//! rejects unknown top-level arguments and ignores unknown nested fields. On
//! failure it renders clear error messages.

use serde_json::{Map, Value};

pub struct Validated {
    pub args: Map<String, Value>,
    /// First NaN/±inf a float field accepted; the request body can't encode it.
    pub non_finite: Option<&'static str>,
}

struct Issue {
    loc: Vec<String>,
    msg: String,
}

#[derive(Default)]
struct Checker {
    issues: Vec<Issue>,
    non_finite: Option<&'static str>,
}

pub fn validate_call(
    tool: &str,
    schema: &Map<String, Value>,
    args: Option<&Map<String, Value>>,
) -> Result<Validated, String> {
    let empty = Map::new();
    let args = args.unwrap_or(&empty);
    let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
    let required = required(schema);
    let mut checker = Checker::default();
    let mut out = Map::new();
    for (name, prop) in properties {
        let prop = prop.as_object().unwrap_or(&empty);
        match args.get(name) {
            Some(value) => {
                let mut loc = vec![name.clone()];
                if let Some(v) = checker.check(prop, value, &mut loc) {
                    out.insert(name.clone(), v);
                }
            }
            None => match prop.get("default") {
                Some(default) => {
                    out.insert(name.clone(), default.clone());
                }
                None if required.contains(&name.as_str()) => checker.issues.push(Issue {
                    loc: vec![name.clone()],
                    msg: "Missing required argument".into(),
                }),
                None => {}
            },
        }
    }
    for (name, _) in args {
        if !properties.contains_key(name) {
            checker.issues.push(Issue {
                loc: vec![name.clone()],
                msg: "Unexpected argument".into(),
            });
        }
    }
    if checker.issues.is_empty() {
        Ok(Validated { args: out, non_finite: checker.non_finite })
    } else {
        Err(report(&format!("call[{tool}]"), &checker.issues))
    }
}

fn required(schema: &Map<String, Value>) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

impl Checker {
    fn fail(&mut self, loc: &[String], msg: impl Into<String>) -> Option<Value> {
        self.issues.push(Issue { loc: loc.to_vec(), msg: msg.into() });
        None
    }

    fn check(&mut self, schema: &Map<String, Value>, value: &Value, loc: &mut Vec<String>) -> Option<Value> {
        if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
            // Only `Optional[X]` appears: X plus {"type": "null"}.
            if value.is_null() {
                return Some(Value::Null);
            }
            let inner = any_of
                .iter()
                .filter_map(Value::as_object)
                .find(|s| s.get("type").and_then(Value::as_str) != Some("null"))?;
            return self.check(inner, value, loc);
        }
        if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
            if choices.contains(value) {
                return Some(value.clone());
            }
            return self.fail(loc, literal_message(choices));
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => match value {
                Value::String(_) => Some(value.clone()),
                _ => self.fail(loc, "Input should be a valid string"),
            },
            Some("integer") => match to_int(value) {
                Ok(n) => Some(Value::from(n)),
                Err(msg) => self.fail(loc, msg),
            },
            Some("number") => match to_float(value) {
                Ok(f) => match serde_json::Number::from_f64(f) {
                    Some(n) => Some(Value::Number(n)),
                    None => {
                        self.non_finite.get_or_insert(if f.is_nan() {
                            "nan"
                        } else if f > 0.0 {
                            "inf"
                        } else {
                            "-inf"
                        });
                        Some(Value::Null)
                    }
                },
                Err(msg) => self.fail(loc, msg),
            },
            Some("boolean") => match to_bool(value) {
                Ok(b) => Some(Value::Bool(b)),
                Err(msg) => self.fail(loc, msg),
            },
            Some("array") => {
                let Value::Array(items) = value else {
                    return self.fail(loc, "Input should be a valid list");
                };
                let item_schema = schema.get("items").and_then(Value::as_object).cloned().unwrap_or_default();
                let mut out = Vec::with_capacity(items.len());
                let mut ok = true;
                for (i, item) in items.iter().enumerate() {
                    loc.push(i.to_string());
                    match self.check(&item_schema, item, loc) {
                        Some(v) => out.push(v),
                        None => ok = false,
                    }
                    loc.pop();
                }
                ok.then_some(Value::Array(out))
            }
            Some("object") => {
                let Value::Object(fields) = value else {
                    return self.fail(loc, "Input should be a valid object");
                };
                let empty = Map::new();
                let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
                let required = required(schema);
                let mut out = Map::new();
                let mut ok = true;
                for (name, prop) in properties {
                    let prop = prop.as_object().unwrap_or(&empty);
                    match fields.get(name) {
                        Some(field) => {
                            loc.push(name.clone());
                            match self.check(prop, field, loc) {
                                Some(v) => {
                                    out.insert(name.clone(), v);
                                }
                                None => ok = false,
                            }
                            loc.pop();
                        }
                        None => match prop.get("default") {
                            Some(default) => {
                                out.insert(name.clone(), default.clone());
                            }
                            None if required.contains(&name.as_str()) => {
                                loc.push(name.clone());
                                self.fail(loc, "Field required");
                                loc.pop();
                                ok = false;
                            }
                            None => {}
                        },
                    }
                }
                ok.then_some(Value::Object(out))
            }
            _ => Some(value.clone()),
        }
    }
}

type Coerced<T> = Result<T, &'static str>;

const INT_TYPE: &str = "Input should be a valid integer";
const INT_PARSING: &str = "Input should be a valid integer, unable to parse string as an integer";
const INT_SIZE: &str = "Unable to parse input string as an integer, exceeded maximum size";
const INT_FLOAT: &str = "Input should be a valid integer, got a number with a fractional part";

fn to_int(value: &Value) -> Coerced<i64> {
    match value {
        Value::Bool(b) => Ok(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return Ok(i);
            }
            if n.is_u64() {
                return Err(INT_SIZE);
            }
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.fract() != 0.0 {
                return Err(INT_FLOAT);
            }
            if (i64::MIN as f64..i64::MAX as f64).contains(&f) {
                Ok(f as i64)
            } else {
                Err(INT_SIZE)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            let s = match s.split_once('.') {
                Some((whole, frac)) if !frac.is_empty() && frac.bytes().all(|b| b == b'0') => whole,
                Some(_) => return Err(INT_PARSING),
                None => s,
            };
            let digits = strip_underscores(s.strip_prefix(['+', '-']).unwrap_or(s)).ok_or(INT_PARSING)?;
            let negative = s.starts_with('-');
            let parsed: i128 = digits.parse().map_err(|_| INT_SIZE)?;
            let parsed = if negative { -parsed } else { parsed };
            i64::try_from(parsed).map_err(|_| INT_SIZE)
        }
        _ => Err(INT_TYPE),
    }
}

fn strip_underscores(s: &str) -> Option<String> {
    if s.is_empty() || s.starts_with('_') || s.ends_with('_') || s.contains("__") {
        return None;
    }
    let digits: String = s.chars().filter(|&c| c != '_').collect();
    digits.bytes().all(|b| b.is_ascii_digit()).then_some(digits).filter(|d| !d.is_empty())
}

const FLOAT_TYPE: &str = "Input should be a valid number";
const FLOAT_PARSING: &str = "Input should be a valid number, unable to parse string as a number";

fn to_float(value: &Value) -> Coerced<f64> {
    match value {
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Number(n) => Ok(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => {
            let s = s.trim();
            let lowered = s.to_ascii_lowercase();
            let unsigned = lowered.strip_prefix(['+', '-']).unwrap_or(&lowered);
            if matches!(unsigned, "inf" | "infinity" | "nan") {
                return lowered.parse::<f64>().map_err(|_| FLOAT_PARSING);
            }
            let bytes = s.as_bytes();
            for (i, &b) in bytes.iter().enumerate() {
                if b == b'_'
                    && !(i > 0
                        && i + 1 < bytes.len()
                        && bytes[i - 1].is_ascii_digit()
                        && bytes[i + 1].is_ascii_digit())
                {
                    return Err(FLOAT_PARSING);
                }
            }
            let cleaned: String = s.chars().filter(|&c| c != '_').collect();
            let valid_chars = cleaned.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b));
            if !valid_chars || !cleaned.bytes().any(|b| b.is_ascii_digit()) {
                return Err(FLOAT_PARSING);
            }
            cleaned.parse::<f64>().map_err(|_| FLOAT_PARSING)
        }
        _ => Err(FLOAT_TYPE),
    }
}

const BOOL_TYPE: &str = "Input should be a valid boolean";
const BOOL_PARSING: &str = "Input should be a valid boolean, unable to interpret input";

fn to_bool(value: &Value) -> Coerced<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return match i {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(BOOL_PARSING),
                };
            }
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.fract() != 0.0 {
                Err(BOOL_TYPE)
            } else if f == 0.0 {
                Ok(false)
            } else if f == 1.0 {
                Ok(true)
            } else {
                Err(BOOL_PARSING)
            }
        }
        Value::String(s) => match s.to_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(BOOL_PARSING),
        },
        _ => Err(BOOL_TYPE),
    }
}

fn literal_message(choices: &[Value]) -> String {
    let reprs: Vec<String> = choices
        .iter()
        .map(|v| match v {
            Value::String(s) => format!("'{s}'"),
            other => other.to_string(),
        })
        .collect();
    let listed = match reprs.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    };
    format!("Input should be {listed}")
}

fn report(title: &str, issues: &[Issue]) -> String {
    let n = issues.len();
    let mut out = format!("{n} validation error{} for {title}:", if n == 1 { "" } else { "s" });
    for issue in issues {
        out.push_str(&format!("\n  {}: {}", issue.loc.join("."), issue.msg));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lax_coercion() {
        for (input, want) in [
            (json!("2"), Ok(2)),
            (json!("2.00"), Ok(2)),
            (json!(" -7 "), Ok(-7)),
            (json!("1_000"), Ok(1000)),
            (json!(true), Ok(1)),
            (json!(2.0), Ok(2)),
            (json!(9.2e18), Ok(9_200_000_000_000_000_000)),
            (json!("2."), Err(INT_PARSING)),
            (json!("1__0"), Err(INT_PARSING)),
            (json!("+-1"), Err(INT_PARSING)),
            (json!(1.5), Err(INT_FLOAT)),
            (json!(1e19), Err(INT_SIZE)),
            (json!(null), Err(INT_TYPE)),
        ] {
            assert_eq!(to_int(&input), want, "{input}");
        }
        for (input, want) in [
            (json!("1_0"), Ok(10.0)),
            (json!(".5"), Ok(0.5)),
            (json!("5."), Ok(5.0)),
            (json!(" 2 "), Ok(2.0)),
            (json!("1e2"), Ok(100.0)),
            (json!(false), Ok(0.0)),
            (json!("_1"), Err(FLOAT_PARSING)),
            (json!("1e"), Err(FLOAT_PARSING)),
            (json!("0x1p3"), Err(FLOAT_PARSING)),
            (json!("-"), Err(FLOAT_PARSING)),
            (json!([]), Err(FLOAT_TYPE)),
        ] {
            assert_eq!(to_float(&input), want, "{input}");
        }
        assert!(to_float(&json!("INF")).unwrap().is_infinite());
        assert!(to_float(&json!("-nan")).unwrap().is_nan());
        for (input, want) in [
            (json!("YES"), Ok(true)),
            (json!("off"), Ok(false)),
            (json!(1.0), Ok(true)),
            (json!(0), Ok(false)),
            (json!(" t "), Err(BOOL_PARSING)),
            (json!(2), Err(BOOL_PARSING)),
            (json!(2.0), Err(BOOL_PARSING)),
            (json!(0.5), Err(BOOL_TYPE)),
            (json!(null), Err(BOOL_TYPE)),
            (json!(""), Err(BOOL_PARSING)),
        ] {
            assert_eq!(to_bool(&input), want, "{input}");
        }
    }
}
