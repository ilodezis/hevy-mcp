//! The handful of Python URL-quoting rules the wire format depends on.

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

fn push_escaped(out: &mut String, c: char) {
    let mut buf = [0u8; 4];
    for b in c.encode_utf8(&mut buf).bytes() {
        out.push_str(&format!("%{b:02X}"));
    }
}

/// `urllib.parse.quote(s, safe=safe)`.
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() && (UNRESERVED.contains(&(c as u8)) || safe.contains(c)) {
            out.push(c);
        } else {
            push_escaped(&mut out, c);
        }
    }
    out
}

/// `urllib.parse.quote_plus(s)` — what `urlencode()` applies to each key/value.
pub fn quote_plus(s: &str) -> String {
    quote(s, " ").replace(' ', "+")
}

/// `urllib.parse.urlencode(pairs)`.
pub fn urlencode<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// httpx's quoting of a URL component: printable ASCII outside `unsafe_ascii`
/// passes through (so existing `%XX` escapes survive), everything else is
/// UTF-8 percent-encoded.
fn httpx_quote(s: &str, unsafe_ascii: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_graphic() && !unsafe_ascii.contains(&(c as u8)) {
            out.push(c);
        } else {
            push_escaped(&mut out, c);
        }
    }
    out
}

/// RFC 3986 `remove_dot_segments`, as httpx applies it to absolute URLs.
fn normalize_path(path: &str) -> String {
    if !path.split('/').any(|seg| seg == "." || seg == "..") {
        return path.to_string();
    }
    let mut output: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => {}
            ".." => {
                if !output.is_empty() && output != [""] {
                    output.pop();
                }
            }
            _ => output.push(seg),
        }
    }
    let mut joined = output.join("/");
    if path.ends_with("/.") || path.ends_with("/..") {
        joined.push('/');
    }
    joined
}

/// Turn `base + path` — where `path` may embed caller-supplied ids — into the
/// URL httpx would request: fragment dropped, dot segments resolved, unsafe
/// characters percent-encoded, and `params` (when given) as the query.
pub fn build_url(base: &str, path: &str, params: &[(&str, String)]) -> String {
    let raw = format!("{}{}", base.trim_end_matches('/'), path);
    let raw = raw.split('#').next().unwrap_or_default();
    let (before_query, query) = match raw.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (raw, None),
    };
    // Split scheme://authority from the path.
    let path_start = before_query
        .find("://")
        .and_then(|i| before_query[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(before_query.len());
    let (origin, path) = before_query.split_at(path_start);
    let mut url = String::from(origin);
    url.push_str(&httpx_quote(&normalize_path(path), b" \"#<>?`{}"));
    // httpx replaces, rather than extends, a query the path already carried.
    let query = if params.is_empty() {
        query.map(|q| httpx_quote(q, b" \"#<>"))
    } else {
        Some(urlencode(params.iter().map(|(k, v)| (*k, v.as_str()))))
    };
    if let Some(query) = query {
        url.push('?');
        url.push_str(&query);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_quoting() {
        assert_eq!(quote_plus("/oauth/authorize?a=1&b=x y~*"), "%2Foauth%2Fauthorize%3Fa%3D1%26b%3Dx+y~%2A");
        assert_eq!(quote("/x y?q=é", ":/%#?=@[]!$&'()*+,;"), "/x%20y?q=%C3%A9");
    }

    #[test]
    fn httpx_url_building() {
        let b = "https://hevy.example/";
        assert_eq!(build_url(b, "/v1/workouts/w1", &[]), "https://hevy.example/v1/workouts/w1");
        assert_eq!(build_url(b, "/v1/workouts/a/b?c d", &[]), "https://hevy.example/v1/workouts/a/b?c%20d");
        assert_eq!(build_url(b, "/v1/workouts/", &[]), "https://hevy.example/v1/workouts/");
        assert_eq!(build_url(b, "/v1/workouts/../routines/x#frag", &[]), "https://hevy.example/v1/routines/x");
        assert_eq!(build_url(b, "/v1/workouts/Жим 100%", &[]), "https://hevy.example/v1/workouts/%D0%96%D0%B8%D0%BC%20100%");
        assert_eq!(
            build_url(b, "/v1/exercise_history/x", &[("start_date", "2026-01-01T00:00:00Z".into())]),
            "https://hevy.example/v1/exercise_history/x?start_date=2026-01-01T00%3A00%3A00Z"
        );
        assert_eq!(
            build_url("http://127.0.0.1:9", "/v1/workouts", &[("page", "1".into()), ("pageSize", "10".into())]),
            "http://127.0.0.1:9/v1/workouts?page=1&pageSize=10"
        );
        assert_eq!(build_url(b, "/v1/x?a=1", &[("b", "2".into())]), "https://hevy.example/v1/x?b=2");
        assert_eq!(build_url(b, "/v1/x{y}`z\"", &[]), "https://hevy.example/v1/x%7By%7D%60z%22");
    }
}
