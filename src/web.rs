//! HTTP surface: health check, the single-user login gate, the OAuth 2.0
//! authorization server for Claude.ai, and the MCP endpoint at `/mcp/` behind
//! a Bearer-token check.
//!
//! Routing reproduces FastAPI/Starlette's observable behaviour — JSON 404 and
//! 405 bodies, 422 for missing form/query fields, a 307 redirect when only the
//! trailing slash differs (`/mcp` → `/mcp/`) — so clients see the same answers
//! they got from the earlier Python server.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}};
use ring::{digest, rand::{SecureRandom, SystemRandom}};
use rmcp::transport::streamable_http_server::{StreamableHttpService, session::local::LocalSessionManager};
use serde_json::{Map, Value, json};

use crate::{
    config::Config,
    hevy::HevyClient,
    mcp::HevyMcp,
    signing::Serializer,
    templates,
    urlenc::{quote, urlencode},
};

const SESSION_COOKIE: &str = "hevy_mcp_session";
const CODE_TTL: Duration = Duration::from_secs(300);
const MAX_FORM_BYTES: usize = 1024 * 1024;

pub struct App {
    pub cfg: Config,
    pub hevy: Arc<HevyClient>,
    pub mcp: StreamableHttpService<HevyMcp, LocalSessionManager>,
    /// Browser session cookie and MCP access token share the secret under
    /// different salts, so neither can be replayed as the other.
    session: Serializer,
    token: Serializer,
    codes: Mutex<HashMap<String, (Instant, CodeData)>>,
    rng: SystemRandom,
    /// Where we listen: the authority for `base_url` when a request has no `Host`.
    local_addr: SocketAddr,
}

#[derive(Clone)]
struct CodeData {
    redirect_uri: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

impl App {
    pub fn new(
        cfg: Config,
        hevy: Arc<HevyClient>,
        mcp: StreamableHttpService<HevyMcp, LocalSessionManager>,
        local_addr: SocketAddr,
    ) -> Self {
        Self {
            session: Serializer::new(&cfg.session_secret, "hevy-mcp-session"),
            token: Serializer::new(&cfg.session_secret, "hevy-mcp-token"),
            cfg,
            hevy,
            mcp,
            codes: Mutex::new(HashMap::new()),
            rng: SystemRandom::new(),
            local_addr,
        }
    }

    fn read_session(&self, headers: &HeaderMap) -> bool {
        cookie(headers, SESSION_COOKIE)
            .and_then(|token| self.session.loads(&token, self.cfg.session_ttl))
            .is_some_and(|payload| payload.is_string())
    }

    fn token_valid(&self, token: &str) -> bool {
        self.token.loads(token, self.cfg.token_ttl).is_some_and(|payload| {
            payload.get("type") == Some(&json!("mcp_access_token"))
                && payload.get("client_id") == Some(&json!(self.cfg.mcp_client_id))
        })
    }

    fn store_code(&self, code: String, data: CodeData) {
        let now = Instant::now();
        let mut codes = self.codes.lock().expect("code store lock");
        // Opportunistic sweep of expired codes.
        codes.retain(|_, (expires, _)| *expires > now);
        codes.insert(code, (now + CODE_TTL, data));
    }

    /// Single-use: the code is removed whether or not it is still valid.
    fn pop_code(&self, code: &str) -> Option<CodeData> {
        let (expires, data) = self.codes.lock().expect("code store lock").remove(code)?;
        (expires > Instant::now()).then_some(data)
    }

    fn token_urlsafe(&self, n: usize) -> String {
        let mut bytes = vec![0u8; n];
        self.rng.fill(&mut bytes).expect("system RNG");
        URL_SAFE_NO_PAD.encode(bytes)
    }
}

/// `secrets.compare_digest`: constant time for equal lengths.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── Responses ────────────────────────────────────────────────────────────────

fn response(status: StatusCode, content_type: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .expect("valid response")
}

fn json_response(status: StatusCode, value: Value) -> Response {
    response(status, "application/json", serde_json::to_vec(&value).expect("JSON value"))
}

fn html(status: StatusCode, page: String) -> Response {
    response(status, "text/html; charset=utf-8", page)
}

fn detail(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({"detail": message}))
}

/// Starlette's RedirectResponse, which percent-quotes the target.
fn redirect(status: StatusCode, url: &str) -> Response {
    let location = quote(url, ":/%#?=@[]!$&'()*+,;");
    Response::builder()
        .status(status)
        .header(header::LOCATION, location)
        .header(header::CONTENT_LENGTH, 0)
        .body(Body::empty())
        .expect("valid response")
}

fn method_not_allowed(allow: &str) -> Response {
    let mut resp = detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    resp.headers_mut().insert(header::ALLOW, HeaderValue::from_str(allow).expect("ascii"));
    resp
}

fn internal_error() -> Response {
    response(StatusCode::INTERNAL_SERVER_ERROR, "text/plain; charset=utf-8", "Internal Server Error")
}

/// FastAPI's 422 for required query/form fields that are absent.
fn missing_fields(source: &str, fields: &[&str]) -> Response {
    let detail: Vec<Value> = fields
        .iter()
        .map(|f| json!({"type": "missing", "loc": [source, f], "msg": "Field required", "input": null}))
        .collect();
    json_response(StatusCode::UNPROCESSABLE_ENTITY, json!({"detail": detail}))
}

/// `http.cookies` output for the session cookie, attributes sorted by name.
fn set_cookie(resp: &mut Response, value: &str, max_age: u64, secure: bool, httponly: bool, samesite: &str) {
    let expires = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(max_age));
    let value = if value.is_empty() { "\"\"" } else { value };
    let mut cookie = format!("{SESSION_COOKIE}={value}; expires={expires}");
    if httponly {
        cookie.push_str("; HttpOnly");
    }
    cookie.push_str(&format!("; Max-Age={max_age}; Path=/; SameSite={samesite}"));
    if secure {
        cookie.push_str("; Secure");
    }
    resp.headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(&cookie).expect("cookie is ascii"));
}

// ── Request helpers ──────────────────────────────────────────────────────────

/// Starlette's cookie parsing of the first `Cookie` header; last duplicate wins.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    let mut found = None;
    for chunk in raw.split(';') {
        let (key, value) = chunk.split_once('=').unwrap_or(("", chunk));
        if key.trim() == name {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
                .unwrap_or_else(|| value.to_string());
            found = Some(value);
        }
    }
    found
}

/// Query/form fields; the last occurrence of a repeated key wins.
fn parse_pairs(raw: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(raw).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

async fn read_body(body: Body) -> Option<bytes::Bytes> {
    to_bytes(body, MAX_FORM_BYTES).await.ok()
}

/// A url-encoded form body; anything else reads as an empty form.
async fn form(headers: &HeaderMap, body: Body) -> HashMap<String, String> {
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/x-www-form-urlencoded"));
    match (is_form, read_body(body).await) {
        (true, Some(raw)) => parse_pairs(&raw),
        _ => HashMap::new(),
    }
}

/// FastAPI `Form(...)` semantics: an empty string counts as not sent.
fn form_field(form: &HashMap<String, String>, name: &str) -> Option<String> {
    form.get(name).filter(|v| !v.is_empty()).cloned()
}

/// `request.base_url` behind uvicorn's `--proxy-headers --forwarded-allow-ips *`:
/// scheme from `X-Forwarded-Proto`, authority from `Host`.
fn base_url(headers: &HeaderMap, local: SocketAddr) -> String {
    let scheme = headers
        .get_all("x-forwarded-proto")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|p| matches!(*p, "http" | "https" | "ws" | "wss"))
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| local.to_string());
    format!("{scheme}://{host}")
}

/// Confine the post-login redirect to a same-site path (open-redirect guard).
fn safe_next(next: Option<&str>) -> String {
    let Some(next) = next.filter(|n| !n.is_empty()) else { return "/".into() };
    if !next.starts_with('/') || next.starts_with("//") || next.contains('\\') {
        return "/".into();
    }
    // urlsplit() drops tab/CR/LF first, so "/\t/evil" would still name a host.
    let cleaned: String = next.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    if let Some(rest) = cleaned.strip_prefix("//")
        && !rest.split(['/', '?', '#']).next().unwrap_or("").is_empty()
    {
        return "/".into();
    }
    next.to_string()
}

/// `urlsplit(url).netloc`.
fn netloc(url: &str) -> &str {
    let rest = match url.find(':') {
        Some(i)
            if i > 0
                && url[..i].starts_with(|c: char| c.is_ascii_alphabetic())
                && url[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            &url[i + 1..]
        }
        _ => url,
    };
    match rest.strip_prefix("//") {
        Some(after) => after.split(['/', '?', '#']).next().unwrap_or(""),
        None => "",
    }
}

fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

// ── Routing ──────────────────────────────────────────────────────────────────

/// Routes in the order the FastAPI app registered them, with their methods.
const ROUTES: &[(&str, &[Method])] = &[
    ("/login", &[Method::GET, Method::POST]),
    ("/logout", &[Method::GET, Method::POST]),
    ("/.well-known/oauth-authorization-server", &[Method::GET]),
    ("/oauth/authorize", &[Method::GET]),
    ("/oauth/authorize/approve", &[Method::POST]),
    ("/oauth/token", &[Method::POST]),
    ("/", &[Method::GET]),
];

fn is_mcp_path(path: &str) -> bool {
    path.starts_with("/mcp/")
}

fn route_exists(path: &str) -> bool {
    is_mcp_path(path) || ROUTES.iter().any(|(p, _)| *p == path)
}

pub async fn handle(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<Response, Infallible> {
    let started = Instant::now();
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();
    let client = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| peer.ip().to_string());
    let resp = route(&app, req).await;
    eprintln!(
        "INFO: {client} - \"{method} {raw_path}\" {} ({} ms)",
        resp.status().as_u16(),
        started.elapsed().as_millis()
    );
    Ok(resp)
}

async fn route(app: &App, req: Request) -> Response {
    let path = percent_decode_path(req.uri().path());
    let method = req.method().clone();
    if is_mcp_path(&path) {
        return mcp_endpoint(app, req, &path).await;
    }
    let local = app.local_addr;
    match (path.as_str(), method.clone()) {
        ("/login", Method::GET) => login_page(app, req),
        ("/login", Method::POST) => login(app, req).await,
        ("/logout", Method::GET | Method::POST) => logout(),
        ("/.well-known/oauth-authorization-server", Method::GET) => oauth_metadata(req.headers(), local),
        ("/oauth/authorize", Method::GET) => oauth_authorize(app, req),
        ("/oauth/authorize/approve", Method::POST) => oauth_approve(app, req).await,
        ("/oauth/token", Method::POST) => oauth_token(app, req).await,
        ("/", Method::GET) => json_response(
            StatusCode::OK,
            json!({"service": "hevy-mcp", "status": "ok", "hevy_configured": app.hevy.is_configured()}),
        ),
        _ => {
            if let Some((_, methods)) = ROUTES.iter().find(|(p, _)| *p == path) {
                return method_not_allowed(methods[0].as_str());
            }
            // Starlette's redirect_slashes: retry with the trailing slash toggled.
            if path != "/" {
                let toggled = match path.strip_suffix('/') {
                    Some(stripped) => stripped.to_string(),
                    None => format!("{path}/"),
                };
                if route_exists(&toggled) {
                    let mut url = format!("{}{}", base_url(req.headers(), local), toggled);
                    if let Some(q) = req.uri().query() {
                        url.push('?');
                        url.push_str(q);
                    }
                    return redirect(StatusCode::TEMPORARY_REDIRECT, &url);
                }
            }
            detail(StatusCode::NOT_FOUND, "Not Found")
        }
    }
}

// ── MCP endpoint ─────────────────────────────────────────────────────────────

async fn mcp_endpoint(app: &App, req: Request, path: &str) -> Response {
    if req.method() == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::OK)
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, POST, OPTIONS")
            .header("access-control-allow-headers", "Authorization, Content-Type")
            .header(header::CONTENT_LENGTH, 0)
            .body(Body::empty())
            .expect("valid response");
    }
    // Bearer header ONLY — never a query param (leaks into logs/history).
    let auth = req
        .headers()
        .get_all(header::AUTHORIZATION)
        .iter()
        .next_back()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let token = auth
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("bearer "))
        .map(|_| &auth[7..]);
    if !token.is_some_and(|t| !t.is_empty() && app.token_valid(t)) {
        let mut resp = detail(StatusCode::UNAUTHORIZED, "Unauthorized. Invalid or missing MCP access token.");
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return resp;
    }
    if path != "/mcp/" {
        return response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", "Not Found");
    }
    let method = req.method().clone();
    let session = req.headers().get("mcp-session-id").cloned();
    let resp = app.mcp.handle(req).await.map(Body::new);
    like_python_sdk(resp, &method, session).await
}

/// Align rmcp's transport-level replies with what the Python SDK sent: the
/// session id echoed on every in-session response, `200` for a session
/// DELETE, a JSON content type on bodiless acks, and JSON-RPC error objects
/// (not plain text) for transport errors.
async fn like_python_sdk(mut resp: Response, method: &Method, session: Option<HeaderValue>) -> Response {
    let status = resp.status();
    if status.is_success() {
        if let Some(session) = session
            && !resp.headers().contains_key("mcp-session-id")
        {
            resp.headers_mut().insert("mcp-session-id", session);
        }
        if *method == Method::DELETE && status == StatusCode::ACCEPTED {
            *resp.status_mut() = StatusCode::OK;
        }
        if !resp.headers().contains_key(header::CONTENT_TYPE) {
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        return resp;
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        let (mut parts, _) = resp.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    }
    if !status.is_client_error() || resp.headers().contains_key(header::CONTENT_TYPE) {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let text = read_body(body).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let message = match text.as_str() {
        "Bad Request: Session ID is required" => "Bad Request: Missing session ID",
        "Not Found: Session not found" => "Session not found",
        other => other,
    };
    let body = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": message}}))
        .expect("JSON value");
    parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}

// ── Login gate ───────────────────────────────────────────────────────────────

fn login_page(app: &App, req: Request) -> Response {
    let query = parse_pairs(req.uri().query().unwrap_or("").as_bytes());
    let next = safe_next(query.get("next").map(String::as_str));
    if app.read_session(req.headers()) {
        return redirect(StatusCode::FOUND, &next);
    }
    html(StatusCode::OK, templates::login(None, &next))
}

async fn login(app: &App, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let form = form(&parts.headers, body).await;
    let Some(password) = form_field(&form, "password") else {
        return missing_fields("body", &["password"]);
    };
    let next = safe_next(form_field(&form, "next").as_deref());
    let cfg = &app.cfg;
    let ok = !cfg.auth_password.is_empty()
        && ct_eq(password.as_bytes(), cfg.auth_password.as_bytes());
    if !ok {
        return html(StatusCode::UNAUTHORIZED, templates::login(Some("Неверный пароль"), &next));
    }
    let mut resp = redirect(StatusCode::SEE_OTHER, &next);
    let session = app.session.dumps(&json!("owner"));
    set_cookie(&mut resp, &session, cfg.session_ttl, cfg.cookie_secure, true, &cfg.cookie_samesite);
    resp
}

fn logout() -> Response {
    let mut resp = redirect(StatusCode::SEE_OTHER, "/login");
    set_cookie(&mut resp, "", 0, false, false, "lax");
    resp
}

// ── OAuth 2.0 authorization server ───────────────────────────────────────────

fn oauth_metadata(headers: &HeaderMap, local: SocketAddr) -> Response {
    let base = base_url(headers, local);
    json_response(
        StatusCode::OK,
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"],
            "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
            "code_challenge_methods_supported": ["S256", "plain"],
        }),
    )
}

fn oauth_authorize(app: &App, req: Request) -> Response {
    let raw_query = req.uri().query().unwrap_or("");
    let q = parse_pairs(raw_query.as_bytes());
    let missing: Vec<&str> = ["response_type", "client_id", "redirect_uri"]
        .into_iter()
        .filter(|k| !q.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return missing_fields("query", &missing);
    }
    let cfg = &app.cfg;
    let redirect_uri = q["redirect_uri"].as_str();
    let error_page = |message: &str| {
        templates::oauth_authorize(&HashMap::from([("error", Some(message)), ("redirect_uri", Some(redirect_uri))]))
    };
    if q["client_id"] != cfg.mcp_client_id {
        return html(StatusCode::OK, error_page("Неизвестный client_id."));
    }
    if q["response_type"] != "code" {
        return html(StatusCode::OK, error_page("Поддерживается только response_type=code."));
    }
    if !cfg.mcp_redirect_uris.iter().any(|u| u == redirect_uri) {
        return html(StatusCode::OK, error_page("redirect_uri не в списке разрешённых."));
    }

    // Require a logged-in owner session; otherwise bounce to login and come back.
    if !app.read_session(req.headers()) {
        let mut next_path = req.uri().path().to_string();
        if !raw_query.is_empty() {
            next_path.push('?');
            next_path.push_str(raw_query);
        }
        let login_url = format!("/login?{}", urlencode([("next", next_path.as_str())]));
        return redirect(StatusCode::FOUND, &login_url);
    }

    html(
        StatusCode::OK,
        templates::oauth_authorize(&HashMap::from([
            ("error", None),
            ("client_id", Some(q["client_id"].as_str())),
            ("redirect_uri", Some(redirect_uri)),
            ("redirect_domain", Some(netloc(redirect_uri))),
            ("state", q.get("state").map(String::as_str)),
            ("code_challenge", q.get("code_challenge").map(String::as_str)),
            ("code_challenge_method", q.get("code_challenge_method").map(String::as_str)),
        ])),
    )
}

async fn oauth_approve(app: &App, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let form = form(&parts.headers, body).await;
    let (client_id, redirect_uri) = (form_field(&form, "client_id"), form_field(&form, "redirect_uri"));
    let (Some(client_id), Some(redirect_uri)) = (&client_id, &redirect_uri) else {
        let missing: Vec<&str> = [("client_id", &client_id), ("redirect_uri", &redirect_uri)]
            .into_iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        return missing_fields("body", &missing);
    };
    if !app.read_session(&parts.headers) {
        return detail(StatusCode::UNAUTHORIZED, "Not authenticated");
    }
    let cfg = &app.cfg;
    if *client_id != cfg.mcp_client_id {
        return detail(StatusCode::BAD_REQUEST, "Invalid client_id");
    }
    if !cfg.mcp_redirect_uris.contains(redirect_uri) {
        return detail(StatusCode::BAD_REQUEST, "redirect_uri not allowed");
    }

    let code = format!("code_{}", app.token_urlsafe(32));
    app.store_code(
        code.clone(),
        CodeData {
            redirect_uri: redirect_uri.clone(),
            code_challenge: form_field(&form, "code_challenge"),
            code_challenge_method: form_field(&form, "code_challenge_method"),
        },
    );
    let state = form_field(&form, "state");
    let mut params = vec![("code", code.as_str())];
    if let Some(state) = &state {
        params.push(("state", state));
    }
    let separator = if redirect_uri.contains('?') { "&" } else { "?" };
    redirect(StatusCode::FOUND, &format!("{redirect_uri}{separator}{}", urlencode(params)))
}

fn oauth_error(error: &str, description: &str) -> Response {
    json_response(StatusCode::BAD_REQUEST, json!({"error": error, "error_description": description}))
}

/// Python truthiness of a request field.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

fn pkce_ok(verifier: &str, challenge: &str, method: Option<&str>) -> bool {
    match method {
        None | Some("plain") => ct_eq(verifier.as_bytes(), challenge.as_bytes()),
        Some("S256") => {
            let calculated = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
            ct_eq(calculated.as_bytes(), challenge.trim_end_matches('=').as_bytes())
        }
        Some(_) => false,
    }
}

async fn oauth_token(app: &App, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("application/json"));
    let body: Map<String, Value> = if is_json {
        let Some(raw) = read_body(body).await else { return internal_error() };
        match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(map)) => map,
            _ => return internal_error(),
        }
    } else {
        form(&parts.headers, body).await.into_iter().map(|(k, v)| (k, Value::String(v))).collect()
    };
    let field = |k: &str| body.get(k).filter(|v| !v.is_null());
    let mut client_id = field("client_id").cloned();
    let mut client_secret = field("client_secret").cloned();

    if !truthy(client_secret.as_ref()) {
        let auth = parts.headers.get(header::AUTHORIZATION).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
        if let Some(auth) = auth
            && auth.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("basic "))
        {
            // b64decode(validate=False): characters outside the alphabet are dropped.
            let cleaned: String = auth[6..]
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
                .collect();
            let decoded = STANDARD.decode(cleaned).ok().and_then(|b| String::from_utf8(b).ok());
            if let Some((cid, csec)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
                if !truthy(client_id.as_ref()) {
                    client_id = Some(Value::String(cid.to_string()));
                }
                client_secret = Some(Value::String(csec.to_string()));
            }
        }
    }

    let cfg = &app.cfg;
    if client_id.as_ref().and_then(Value::as_str) != Some(cfg.mcp_client_id.as_str()) {
        return oauth_error("invalid_client", "Client ID mismatch");
    }
    if cfg.mcp_client_secret.is_empty() {
        return oauth_error("invalid_client", "Client secret not configured");
    }
    let secret = match &client_secret {
        Some(Value::String(s)) => s.as_str(),
        other if !truthy(other.as_ref()) => "",
        _ => return internal_error(),
    };
    if !ct_eq(secret.as_bytes(), cfg.mcp_client_secret.as_bytes()) {
        return oauth_error("invalid_client", "Client secret mismatch");
    }
    if field("grant_type").and_then(Value::as_str) != Some("authorization_code") {
        return oauth_error("unsupported_grant_type", "Only authorization_code is supported");
    }
    let code = field("code");
    if !truthy(code) {
        return oauth_error("invalid_request", "Missing code");
    }
    let code_data = match code {
        Some(Value::String(code)) => app.pop_code(code),
        Some(Value::Array(_) | Value::Object(_)) => return internal_error(),
        _ => None,
    };
    let Some(code_data) = code_data else {
        return oauth_error("invalid_grant", "Code expired or invalid");
    };
    let redirect_uri = field("redirect_uri").and_then(Value::as_str);
    if !redirect_uri.is_some_and(|r| cfg.mcp_redirect_uris.iter().any(|u| u == r) && r == code_data.redirect_uri) {
        return oauth_error("invalid_grant", "Redirect URI mismatch");
    }
    if let Some(challenge) = code_data.code_challenge.as_deref() {
        let verifier = field("code_verifier");
        if !truthy(verifier) {
            return oauth_error("invalid_grant", "Missing code_verifier");
        }
        let Some(verifier) = verifier.and_then(Value::as_str) else { return internal_error() };
        if !pkce_ok(verifier, challenge, code_data.code_challenge_method.as_deref()) {
            return oauth_error("invalid_grant", "PKCE verification failed");
        }
    }

    let access_token = app.token.dumps(&json!({"client_id": cfg.mcp_client_id, "type": "mcp_access_token"}));
    json_response(
        StatusCode::OK,
        json!({"access_token": access_token, "token_type": "Bearer", "expires_in": cfg.token_ttl}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_next_guards_open_redirects() {
        assert_eq!(safe_next(Some("/oauth/authorize?x=1")), "/oauth/authorize?x=1");
        for bad in [None, Some(""), Some("https://evil"), Some("//evil"), Some("/\\evil"), Some("/\t/evil"), Some("evil")] {
            assert_eq!(safe_next(bad), "/", "{bad:?}");
        }
        assert_eq!(safe_next(Some("/\t/")), "/\t/");
    }

    #[test]
    fn netloc_like_urlsplit() {
        assert_eq!(netloc("https://claude.ai/api/mcp/auth_callback"), "claude.ai");
        assert_eq!(netloc("https://u:p@host:8080/x?y"), "u:p@host:8080");
        assert_eq!(netloc("//host/x"), "host");
        assert_eq!(netloc("no-scheme"), "");
    }

    #[test]
    fn pkce() {
        let verifier = "a".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        assert!(pkce_ok(&verifier, &challenge, Some("S256")));
        assert!(pkce_ok(&verifier, &format!("{challenge}="), Some("S256")));
        assert!(!pkce_ok("b", &challenge, Some("S256")));
        assert!(pkce_ok("x", "x", None) && pkce_ok("x", "x", Some("plain")));
        assert!(!pkce_ok("x", "x", Some("S512")));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; hevy_mcp_session=tok.en; hevy_mcp_session=\"last\""));
        assert_eq!(cookie(&h, SESSION_COOKIE).as_deref(), Some("last"));
    }
}
