//! Configuration from the environment, with `.env` loaded first.
//!
//! The `.env` reader follows python-dotenv's grammar rather than a Rust
//! crate's, so the same file means the same thing to both servers: quoting,
//! inline comments, escapes and `${VAR}` / `${VAR:-default}` interpolation all
//! resolve identically. That matters most for the consent password — a `$` or
//! `#` in it must not silently change between the two implementations.

use std::{collections::HashMap, env, fs, path::Path};

pub const DEFAULT_REDIRECT_URI: &str = "https://claude.ai/api/mcp/auth_callback";
pub const DEFAULT_HEVY_BASE_URL: &str = "https://api.hevyapp.com";

#[derive(Clone, Debug)]
pub struct Config {
    pub hevy_api_key: String,
    pub hevy_base_url: String,
    /// Plaintext, constant-time compared on the consent page.
    pub auth_password: String,
    pub session_secret: String,
    pub cookie_secure: bool,
    pub cookie_samesite: String,
    pub session_ttl: u64,
    pub token_ttl: u64,
    pub mcp_client_id: String,
    pub mcp_client_secret: String,
    pub mcp_redirect_uris: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let session_secret = getenv("HEVY_MCP_SESSION_SECRET", "");
        if session_secret.is_empty() {
            // Fail loud rather than silently signing tokens with an empty key.
            return Err("HEVY_MCP_SESSION_SECRET is not set — generate one with \
                 `openssl rand -hex 32`"
                .into());
        }
        let cookie_samesite = getenv("HEVY_MCP_COOKIE_SAMESITE", "lax");
        if !matches!(cookie_samesite.to_ascii_lowercase().as_str(), "strict" | "lax" | "none") {
            return Err("HEVY_MCP_COOKIE_SAMESITE must be 'strict', 'lax' or 'none'".into());
        }
        Ok(Self {
            hevy_api_key: getenv("HEVY_API_KEY", ""),
            hevy_base_url: getenv("HEVY_BASE_URL", DEFAULT_HEVY_BASE_URL),
            auth_password: getenv("HEVY_MCP_PASSWORD", ""),
            session_secret,
            cookie_secure: env_bool("HEVY_MCP_COOKIE_SECURE", true),
            cookie_samesite,
            session_ttl: 60 * 60 * 24 * 30, // 30 days for the browser session
            token_ttl: 60 * 60 * 24 * 365,  // 1 year for the MCP access token
            mcp_client_id: getenv("HEVY_MCP_CLIENT_ID", "hevy-claude-connector"),
            mcp_client_secret: getenv("HEVY_MCP_CLIENT_SECRET", ""),
            mcp_redirect_uris: env_csv("HEVY_MCP_REDIRECT_URIS", &[DEFAULT_REDIRECT_URI]),
        })
    }
}

/// `os.getenv(name, default)`: a variable that is set but empty stays empty.
fn getenv(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Err(_) => default,
        Ok(raw) => matches!(raw.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    }
}

fn env_csv(name: &str, default: &[&str]) -> Vec<String> {
    match env::var(name) {
        Ok(raw) if !raw.is_empty() => raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect(),
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// Load the first `.env` found walking up from the working directory.
/// Variables already in the environment win, as with `load_dotenv()`.
pub fn load_dotenv() {
    let Ok(mut dir) = env::current_dir() else { return };
    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            load_dotenv_file(&candidate);
            return;
        }
        if !dir.pop() {
            return;
        }
    }
}

fn load_dotenv_file(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else { return };
    let os_env: HashMap<String, String> = env::vars().collect();
    for (key, value) in resolve(parse_dotenv(&text), &os_env) {
        if let Some(value) = value
            && env::var_os(&key).is_none()
        {
            // SAFETY: called once from main before the runtime starts any thread.
            unsafe { env::set_var(key, value) };
        }
    }
}

/// `${NAME}` / `${NAME:-default}` expansion; the process environment takes
/// precedence over earlier values from the file (python-dotenv, override=False).
fn resolve(
    bindings: Vec<(String, Option<String>)>,
    os_env: &HashMap<String, String>,
) -> Vec<(String, Option<String>)> {
    let mut resolved: Vec<(String, Option<String>)> = Vec::new();
    for (key, value) in bindings {
        let value = value.map(|value| {
            let lookup = |name: &str| -> Option<String> {
                if let Some(v) = os_env.get(name) {
                    return Some(v.clone());
                }
                resolved
                    .iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone().unwrap_or_default())
            };
            interpolate(&value, lookup)
        });
        resolved.push((key, value));
    }
    resolved
}

fn interpolate(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(close) = after.find('}') else { break };
        let body = &after[..close];
        let (name, default) = match body.find(":-") {
            Some(i) if !body[..i].contains(':') => (&body[..i], Some(&body[i + 2..])),
            _ if body.contains(':') => {
                // `[^\}:]*` stops at a bare colon: not a variable reference.
                out.push_str(&rest[..start + 2]);
                rest = after;
                continue;
            }
            _ => (body, None),
        };
        out.push_str(&rest[..start]);
        out.push_str(&lookup(name).unwrap_or_else(|| default.unwrap_or("").to_string()));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// python-dotenv's line grammar, returning `(key, value)` in file order.
/// `value` is `None` for a bare `KEY` with no `=`.
pub fn parse_dotenv(text: &str) -> Vec<(String, Option<String>)> {
    let chars: Vec<char> = text.strip_prefix('\u{feff}').unwrap_or(text).chars().collect();
    let mut p = Parser { s: &chars, i: 0 };
    let mut out = Vec::new();
    while p.i < chars.len() {
        let mark = p.i;
        match p.binding() {
            Ok(Some(binding)) => out.push(binding),
            Ok(None) => {}
            Err(()) => {
                // Invalid line: skip the rest of it, like python-dotenv does.
                p.i = mark;
                while p.i < chars.len() && !matches!(chars[p.i], '\n' | '\r') {
                    p.i += 1;
                }
                p.eol();
            }
        }
    }
    out
}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

fn is_inline_ws(c: char) -> bool {
    c.is_whitespace() && c != '\n' && c != '\r'
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn skip_while(&mut self, f: impl Fn(char) -> bool) -> usize {
        let start = self.i;
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
        self.i - start
    }

    fn eol(&mut self) {
        match self.peek() {
            Some('\r') => {
                self.i += 1;
                if self.peek() == Some('\n') {
                    self.i += 1;
                }
            }
            Some('\n') => self.i += 1,
            _ => {}
        }
    }

    fn binding(&mut self) -> Result<Option<(String, Option<String>)>, ()> {
        self.skip_while(char::is_whitespace);
        if self.peek().is_none() {
            return Ok(None);
        }
        // Optional `export ` prefix.
        let save = self.i;
        let word: String = self.s[self.i..].iter().take(6).collect();
        if word == "export" {
            self.i += 6;
            if self.skip_while(is_inline_ws) == 0 {
                self.i = save;
            }
        }
        let key = match self.peek() {
            Some('#') => None,
            Some('\'') => {
                self.i += 1;
                let start = self.i;
                self.skip_while(|c| c != '\'');
                if self.i == start || self.peek() != Some('\'') {
                    return Err(());
                }
                let key: String = self.s[start..self.i].iter().collect();
                self.i += 1;
                Some(key)
            }
            _ => {
                let start = self.i;
                self.skip_while(|c| c != '=' && c != '#' && !c.is_whitespace());
                if self.i == start {
                    return Err(());
                }
                Some(self.s[start..self.i].iter().collect())
            }
        };
        self.skip_while(is_inline_ws);
        let value = if self.peek() == Some('=') {
            self.i += 1;
            let ws = self.skip_while(is_inline_ws);
            if ws > 0 && self.peek() == Some('#') {
                Some(String::new())
            } else {
                Some(self.value()?)
            }
        } else {
            None
        };
        // Trailing comment, then end of line.
        let save = self.i;
        self.skip_while(is_inline_ws);
        if self.peek() == Some('#') {
            self.skip_while(|c| c != '\n' && c != '\r');
        } else {
            self.i = save;
        }
        self.skip_while(is_inline_ws);
        match self.peek() {
            None | Some('\n') | Some('\r') => self.eol(),
            Some(_) => return Err(()),
        }
        Ok(key.map(|k| (k, value)))
    }

    fn value(&mut self) -> Result<String, ()> {
        match self.peek() {
            Some(q @ ('\'' | '"')) => {
                self.i += 1;
                let mut raw = String::new();
                loop {
                    match self.peek() {
                        None => return Err(()),
                        Some('\\') if self.i + 1 < self.s.len() => {
                            raw.push('\\');
                            raw.push(self.s[self.i + 1]);
                            self.i += 2;
                        }
                        Some('\\') => return Err(()),
                        Some(c) if c == q => {
                            self.i += 1;
                            break;
                        }
                        Some(c) => {
                            raw.push(c);
                            self.i += 1;
                        }
                    }
                }
                Ok(decode_escapes(&raw, q == '"'))
            }
            None | Some('\n') | Some('\r') => Ok(String::new()),
            Some(_) => {
                let start = self.i;
                self.skip_while(|c| c != '\n' && c != '\r');
                let part: String = self.s[start..self.i].iter().collect();
                Ok(strip_inline_comment(&part).trim_end().to_string())
            }
        }
    }
}

/// `re.sub(r"\s+#.*", "", part)`: drop from the first whitespace run that is
/// followed by `#`.
fn strip_inline_comment(part: &str) -> &str {
    let chars: Vec<(usize, char)> = part.char_indices().collect();
    for (n, &(idx, c)) in chars.iter().enumerate() {
        if c.is_whitespace() {
            let mut m = n;
            while m < chars.len() && chars[m].1.is_whitespace() {
                m += 1;
            }
            if m < chars.len() && chars[m].1 == '#' {
                return &part[..idx];
            }
        }
    }
    part
}

fn decode_escapes(raw: &str, double: bool) -> String {
    let mut out = String::new();
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&next) = it.peek() else {
            out.push(c);
            continue;
        };
        let decoded = match (double, next) {
            (_, '\\') => Some('\\'),
            (_, '\'') => Some('\''),
            (true, '"') => Some('"'),
            (true, 'a') => Some('\x07'),
            (true, 'b') => Some('\x08'),
            (true, 'f') => Some('\x0c'),
            (true, 'n') => Some('\n'),
            (true, 'r') => Some('\r'),
            (true, 't') => Some('\t'),
            (true, 'v') => Some('\x0b'),
            _ => None,
        };
        match decoded {
            Some(d) => {
                out.push(d);
                it.next();
            }
            None => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Vec<(String, Option<String>)> {
        resolve(parse_dotenv(text), &HashMap::new())
    }

    fn kv(k: &str, v: &str) -> (String, Option<String>) {
        (k.to_string(), Some(v.to_string()))
    }

    #[test]
    fn dotenv_matches_python_dotenv() {
        // Expected values produced by python-dotenv 1.2.4 on the same input.
        let text = "# comment\n\
            A=plain\n\
            B=with spaces   # trailing comment\n\
            C=pa$word#not-a-comment\n\
            export D='single ${A} \\' q'\n\
            E=\"double\\n${A}\\t\\\"x\\\"\"\n\
            F= # empty\n\
            G=${MISSING:-fallback}/${A}\n\
            H\n\
            I=\n\
            'J K'=quoted key\n\
            bad line here\n\
            L=last\r\n";
        assert_eq!(
            parsed(text),
            vec![
                kv("A", "plain"),
                kv("B", "with spaces"),
                kv("C", "pa$word#not-a-comment"),
                kv("D", "single plain ' q"),
                kv("E", "double\nplain\t\"x\""),
                kv("F", ""),
                kv("G", "fallback/plain"),
                ("H".to_string(), None),
                kv("I", ""),
                kv("J K", "quoted key"),
                kv("L", "last"),
            ]
        );
    }
}
