//! Tool-argument validation that behaves like pydantic v2 in lax mode, which
//! is what FastMCP ran every call through.
//!
//! Driven by the tool's own `inputSchema`, it coerces the way pydantic does
//! (`"2"` → 2, `2.0` → 2, `"yes"` → true, `"62.5"` → 62.5), fills defaults,
//! rejects unknown top-level arguments and ignores unknown nested fields. On
//! failure it renders pydantic's error report, so a model that learned to read
//! those messages keeps reading the same text.

use serde_json::{Map, Value};

pub struct Validated {
    pub args: Map<String, Value>,
    /// First NaN/±inf a float field accepted (pydantic allows `"inf"`); the
    /// request body can't encode it, which Python reports only at send time.
    pub non_finite: Option<&'static str>,
}

struct Issue {
    loc: Vec<String>,
    kind: &'static str,
    msg: String,
    input: Value,
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
                    kind: "missing_argument",
                    msg: "Missing required argument".into(),
                    input: Value::Object(args.clone()),
                }),
                None => {}
            },
        }
    }
    for (name, value) in args {
        if !properties.contains_key(name) {
            checker.issues.push(Issue {
                loc: vec![name.clone()],
                kind: "unexpected_keyword_argument",
                msg: "Unexpected keyword argument".into(),
                input: value.clone(),
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

/// Nested models are anonymous in the schema; pydantic names them in errors.
fn model_name(loc: &[String]) -> &'static str {
    match loc.iter().rev().find(|seg| seg.parse::<usize>().is_err()).map(String::as_str) {
        Some("sets") => "SetInput",
        Some("exercises") => "ExerciseInput",
        _ => "Model",
    }
}

impl Checker {
    fn fail(&mut self, loc: &[String], kind: &'static str, msg: impl Into<String>, input: &Value) -> Option<Value> {
        self.issues.push(Issue { loc: loc.to_vec(), kind, msg: msg.into(), input: input.clone() });
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
            return self.fail(loc, "literal_error", literal_message(choices), value);
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => match value {
                Value::String(_) => Some(value.clone()),
                _ => self.fail(loc, "string_type", "Input should be a valid string", value),
            },
            Some("integer") => match to_int(value) {
                Ok(n) => Some(Value::from(n)),
                Err((kind, msg)) => self.fail(loc, kind, msg, value),
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
                Err((kind, msg)) => self.fail(loc, kind, msg, value),
            },
            Some("boolean") => match to_bool(value) {
                Ok(b) => Some(Value::Bool(b)),
                Err((kind, msg)) => self.fail(loc, kind, msg, value),
            },
            Some("array") => {
                let Value::Array(items) = value else {
                    return self.fail(loc, "list_type", "Input should be a valid list", value);
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
                    let msg = format!("Input should be a valid dictionary or instance of {}", model_name(loc));
                    return self.fail(loc, "model_type", msg, value);
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
                                self.fail(loc, "missing", "Field required", value);
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

type Coerced<T> = Result<T, (&'static str, &'static str)>;

const INT_TYPE: (&str, &str) = ("int_type", "Input should be a valid integer");
const INT_PARSING: (&str, &str) =
    ("int_parsing", "Input should be a valid integer, unable to parse string as an integer");
const INT_SIZE: (&str, &str) =
    ("int_parsing_size", "Unable to parse input string as an integer, exceeded maximum size");

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
                return Err(("int_from_float", "Input should be a valid integer, got a number with a fractional part"));
            }
            // [-2^63, 2^63): `i64::MAX as f64` rounds up to 2^63.
            if (i64::MIN as f64..i64::MAX as f64).contains(&f) {
                Ok(f as i64)
            } else {
                Err(INT_SIZE)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            // `"2.00"` is an integer to pydantic; `"2."` and `"2.5"` are not.
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

/// Python numeric-literal underscores: only single ones, only between digits.
/// Returns the digits with underscores removed, or `None` if malformed.
fn strip_underscores(s: &str) -> Option<String> {
    if s.is_empty() || s.starts_with('_') || s.ends_with('_') || s.contains("__") {
        return None;
    }
    let digits: String = s.chars().filter(|&c| c != '_').collect();
    digits.bytes().all(|b| b.is_ascii_digit()).then_some(digits).filter(|d| !d.is_empty())
}

fn to_float(value: &Value) -> Coerced<f64> {
    const FLOAT_PARSING: (&str, &str) =
        ("float_parsing", "Input should be a valid number, unable to parse string as a number");
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
            // Underscores may only sit between two digits.
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
        _ => Err(("float_type", "Input should be a valid number")),
    }
}

fn to_bool(value: &Value) -> Coerced<bool> {
    const BOOL_TYPE: (&str, &str) = ("bool_type", "Input should be a valid boolean");
    const BOOL_PARSING: (&str, &str) =
        ("bool_parsing", "Input should be a valid boolean, unable to interpret input");
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
    let reprs: Vec<String> = choices.iter().map(py_repr).collect();
    let listed = match reprs.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    };
    format!("Input should be {listed}")
}

fn report(title: &str, issues: &[Issue]) -> String {
    let n = issues.len();
    let mut out = format!("{n} validation error{} for {title}", if n == 1 { "" } else { "s" });
    for issue in issues {
        out.push_str(&format!(
            "\n{}\n  {} [type={}, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/2.14/v/{}",
            issue.loc.join("."),
            issue.msg,
            issue.kind,
            truncate(&py_repr(&issue.input)),
            py_type(&issue.input),
            issue.kind,
        ));
    }
    out
}

/// pydantic-core shows at most 50 bytes of the repr: 25 from the front,
/// `...`, then the tail, cut on character boundaries.
fn truncate(repr: &str) -> String {
    if repr.len() <= 50 {
        return repr.to_string();
    }
    let mut head = 25;
    while !repr.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = repr.len() - 24;
    while !repr.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}...{}", &repr[..head], &repr[tail..])
}

fn py_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python's `repr()` of the value `json.loads` would have produced.
pub fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        Value::Number(n) => py_float_repr(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `repr(float)`: shortest round-trip digits, scientific notation outside
/// 1e-4 <= |x| < 1e16, always a `.0` on integral values.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits = mantissa.replace('.', "");
    let body = if !(-4..16).contains(&exp) {
        let frac = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        format!("{}{frac}e{}{:02}", &digits[..1], if exp < 0 { '-' } else { '+' }, exp.abs())
    } else if exp >= 0 {
        let whole = exp as usize + 1;
        if digits.len() > whole {
            format!("{}.{}", &digits[..whole], &digits[whole..])
        } else {
            format!("{digits}{}.0", "0".repeat(whole - digits.len()))
        }
    } else {
        format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
    };
    format!("{sign}{body}")
}

fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let cp = c as u32;
                if cp <= 0xff {
                    out.push_str(&format!("\\x{cp:02x}"));
                } else if cp <= 0xffff {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    out.push_str(&format!("\\U{cp:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// An approximation of `str.isprintable()`: controls, separators other than
/// the space, format characters and private-use code points are escaped.
fn is_printable(c: char) -> bool {
    let cp = c as u32;
    !matches!(cp,
        0x00..=0x1f | 0x7f..=0xa0 | 0xad
        | 0x0600..=0x0605 | 0x061c | 0x06dd | 0x070f | 0x180e
        | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x206f | 0x3000
        | 0xe000..=0xf8ff | 0xfeff | 0xfff0..=0xfffb
        | 0xf0000..=0x10ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        for (f, want) in [
            (1e16, "1e+16"), (1e15, "1000000000000000.0"), (0.0001, "0.0001"), (0.00001, "1e-05"),
            (123456789012345680.0, "1.2345678901234568e+17"), (-0.0, "-0.0"), (2.5e-300, "2.5e-300"),
            (100.0, "100.0"), (1.5, "1.5"), (1e20, "1e+20"), (1.5e-7, "1.5e-07"), (62.5, "62.5"),
        ] {
            assert_eq!(py_float_repr(f), want);
        }
    }

    #[test]
    fn repr_and_truncation_match_pydantic() {
        assert_eq!(py_repr(&json!({"a": [1, 2.5, true, null, "it's"]})), "{'a': [1, 2.5, True, None, \"it's\"]}");
        assert_eq!(py_repr(&json!("it's \"q\"")), "'it\\'s \"q\"'");
        assert_eq!(py_repr(&json!("\u{a0}\u{ad}\u{200b}\u{2028}\u{e000}😀\u{7f}\u{85} x")), "'\\xa0\\xad\\u200b\\u2028\\ue000😀\\x7f\\x85 x'");
        assert_eq!(truncate(&py_repr(&json!("a".repeat(48)))), format!("'{}'", "a".repeat(48)));
        assert_eq!(truncate(&py_repr(&json!("a".repeat(49)))), format!("'{}...{}'", "a".repeat(24), "a".repeat(23)));
        assert_eq!(truncate(&py_repr(&json!("Ж".repeat(52)))), format!("'{}...{}'", "Ж".repeat(12), "Ж".repeat(11)));
    }

    #[test]
    fn lax_coercion() {
        for (input, want) in [(json!("2"), Ok(2)), (json!("2.00"), Ok(2)), (json!(" -7 "), Ok(-7)), (json!("1_000"), Ok(1000)),
            (json!(true), Ok(1)), (json!(2.0), Ok(2)), (json!(9.2e18), Ok(9_200_000_000_000_000_000)),
            (json!("2."), Err("int_parsing")), (json!("1__0"), Err("int_parsing")), (json!("+-1"), Err("int_parsing")),
            (json!(1.5), Err("int_from_float")), (json!(1e19), Err("int_parsing_size")), (json!(null), Err("int_type"))] {
            assert_eq!(to_int(&input).map_err(|e| e.0), want, "{input}");
        }
        for (input, want) in [(json!("1_0"), Ok(10.0)), (json!(".5"), Ok(0.5)), (json!("5."), Ok(5.0)), (json!(" 2 "), Ok(2.0)),
            (json!("1e2"), Ok(100.0)), (json!(false), Ok(0.0)), (json!("_1"), Err("float_parsing")), (json!("1e"), Err("float_parsing")),
            (json!("0x1p3"), Err("float_parsing")), (json!("-"), Err("float_parsing")), (json!([]), Err("float_type"))] {
            assert_eq!(to_float(&input).map_err(|e| e.0), want, "{input}");
        }
        assert!(to_float(&json!("INF")).unwrap().is_infinite());
        assert!(to_float(&json!("-nan")).unwrap().is_nan());
        for (input, want) in [(json!("YES"), Ok(true)), (json!("off"), Ok(false)), (json!(1.0), Ok(true)), (json!(0), Ok(false)),
            (json!(" t "), Err("bool_parsing")), (json!(2), Err("bool_parsing")), (json!(2.0), Err("bool_parsing")),
            (json!(0.5), Err("bool_type")), (json!(null), Err("bool_type")), (json!(""), Err("bool_parsing"))] {
            assert_eq!(to_bool(&input).map_err(|e| e.0), want, "{input}");
        }
    }
}
