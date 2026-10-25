//! The two consent-flow pages, rendered from the original Jinja templates.
//!
//! The templates use only `{{ var }}`, `{{ var or '' }}` and
//! `{% if var %}…{% else %}…{% endif %}`, so a tiny renderer reproduces
//! Jinja2's output byte for byte: autoescaping as markupsafe does it, block
//! tags leaving their surrounding whitespace in place, and the file's final
//! newline dropped (Jinja's `keep_trailing_newline=False`).

use std::collections::HashMap;

const LOGIN: &str = include_str!("../templates/login.html");
const OAUTH_AUTHORIZE: &str = include_str!("../templates/oauth_authorize.html");

pub type Context<'a> = HashMap<&'static str, Option<&'a str>>;

pub fn login(error: Option<&str>, next: &str) -> String {
    render(LOGIN, &HashMap::from([("error", error), ("next", Some(next))]))
}

pub fn oauth_authorize(ctx: &Context) -> String {
    render(OAUTH_AUTHORIZE, ctx)
}

/// markupsafe's escape.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn render(template: &str, ctx: &Context) -> String {
    let template = template.strip_suffix('\n').unwrap_or(template);
    let truthy = |name: &str| ctx.get(name).copied().flatten().is_some_and(|v| !v.is_empty());
    let mut out = String::with_capacity(template.len() + 256);
    // One entry per open `{% if %}`: (this branch is live, enclosing scope is live).
    let mut stack: Vec<(bool, bool)> = Vec::new();
    let live = |stack: &[(bool, bool)]| stack.last().is_none_or(|&(branch, outer)| branch && outer);
    let mut rest = template;
    while !rest.is_empty() {
        let next_var = rest.find("{{");
        let next_tag = rest.find("{%");
        let (start, is_tag) = match (next_var, next_tag) {
            (None, None) => {
                if live(&stack) {
                    out.push_str(rest);
                }
                break;
            }
            (Some(v), Some(t)) if t < v => (t, true),
            (Some(v), _) => (v, false),
            (None, Some(t)) => (t, true),
        };
        if live(&stack) {
            out.push_str(&rest[..start]);
        }
        let close = if is_tag { "%}" } else { "}}" };
        let end = rest[start..].find(close).expect("unterminated template tag") + start;
        let inner = rest[start + 2..end].trim();
        rest = &rest[end + 2..];
        if is_tag {
            let mut words = inner.split_whitespace();
            match (words.next(), words.next()) {
                (Some("if"), Some(name)) => {
                    let outer = live(&stack);
                    stack.push((truthy(name), outer));
                }
                (Some("else"), None) => {
                    let (branch, outer) = stack.pop().expect("else without if");
                    stack.push((!branch, outer));
                }
                (Some("endif"), None) => {
                    stack.pop().expect("endif without if");
                }
                _ => panic!("unsupported template tag: {inner}"),
            }
        } else if live(&stack) {
            let name = inner.strip_suffix("or ''").map(str::trim).unwrap_or(inner);
            let value = ctx.get(name).copied().flatten().unwrap_or(if inner == name { "None" } else { "" });
            out.push_str(&escape(value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_matches_jinja() {
        let page = login(None, "/oauth/authorize?a=1&b=<x>\"'");
        assert!(page.ends_with("</html>"));
        assert!(page.contains("    <p class=\"sub\">Коннектор Claude ↔ Hevy</p>\n    \n    <input"));
        assert!(page.contains("value=\"/oauth/authorize?a=1&amp;b=&lt;x&gt;&#34;&#39;\""));
        let page = login(Some("Неверный пароль"), "/x");
        assert!(page.contains("    <div class=\"err\" role=\"alert\">Неверный пароль</div>\n    <input"));
    }

    #[test]
    fn consent_page_branches() {
        let error = oauth_authorize(&HashMap::from([("error", Some("Неизвестный client_id."))]));
        assert!(error.contains("  <main class=\"card\">\n    \n    <div class=\"pair broken\""));
        assert!(error.contains("    <div class=\"err\" role=\"alert\">Неизвестный client_id.</div>\n"));
        assert!(error.contains(" на сервере.</p>\n    \n  </main>"));
        assert!(!error.contains("<form"));
        let ok = oauth_authorize(&HashMap::from([
            ("error", None),
            ("client_id", Some("c")),
            ("redirect_uri", Some("https://x/cb")),
            ("redirect_domain", Some("x")),
            ("state", None),
            ("code_challenge", Some("abc")),
            ("code_challenge_method", None),
        ]));
        assert!(ok.contains("name=\"state\" value=\"\""));
        assert!(ok.contains("name=\"code_challenge\" value=\"abc\""));
        assert!(ok.contains("<span class=\"domain\">x</span>"));
        assert!(!ok.contains("Не получилось"));
    }
}
