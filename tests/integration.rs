//! End-to-end checks against the real binary.
//!
//! The server runs as a child process on a loopback port, and the Hevy API it
//! calls is a local mock, so no real account or credentials are involved. The
//! tests exercise the HTTP mount, the OAuth flow, both MCP protocol revisions
//! and every tool, the same way Claude does.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, header},
    response::Response,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use ring::digest;
use serde_json::{Value, json};

// The token and cookie format is part of the contract (existing Claude
// connections must keep working), so the tests mint and read credentials with
// the server's own serializer.
#[allow(dead_code)]
#[path = "../src/signing.rs"]
mod signing;

const SECRET: &str = "integration-test-signing-secret";
const PASSWORD: &str = "integration-password";
const CLIENT_ID: &str = "integration-client";
const CLIENT_SECRET: &str = "integration-client-secret";
const REDIRECT_URI: &str = "https://client.example/callback";
const API_KEY: &str = "integration-api-key";
const ACCEPT: &str = "application/json, text/event-stream";
const LEGACY: &str = "2025-06-18";
const MODERN: &str = "2026-07-28";
const TIMEOUT: Duration = Duration::from_secs(30);

fn token_serializer() -> signing::Serializer {
    signing::Serializer::new(SECRET, "hevy-mcp-token")
}

fn session_serializer() -> signing::Serializer {
    signing::Serializer::new(SECRET, "hevy-mcp-session")
}

fn mint_token(client_id: &str) -> String {
    token_serializer().dumps(&json!({"type": "mcp_access_token", "client_id": client_id}))
}

fn query(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

// ── Mock Hevy ────────────────────────────────────────────────────────────────

struct Seen {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    api_key: Option<String>,
    body: Vec<u8>,
}

struct MockState {
    seen: Mutex<Vec<Seen>>,
    reply: Mutex<(u16, Vec<u8>)>,
}

/// A tiny HTTP server standing in for api.hevyapp.com: it records every
/// request and answers with whatever the test last asked for.
struct Hevy {
    url: String,
    state: Arc<MockState>,
}

impl Hevy {
    async fn start() -> Self {
        let state = Arc::new(MockState {
            seen: Mutex::new(Vec::new()),
            reply: Mutex::new((200, b"{}".to_vec())),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the mock");
        let url = format!("http://{}", listener.local_addr().expect("mock address"));
        let app = Router::new().fallback(mock_handler).with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, state }
    }

    fn reply_json(&self, status: u16, body: &Value) {
        *self.state.reply.lock().expect("mock lock") = (status, serde_json::to_vec(body).expect("JSON"));
    }

    fn reply_text(&self, status: u16, body: &str) {
        *self.state.reply.lock().expect("mock lock") = (status, body.as_bytes().to_vec());
    }

    /// The requests seen since the last call.
    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.state.seen.lock().expect("mock lock"))
    }
}

async fn mock_handler(State(state): State<Arc<MockState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap_or_default();
    let query: BTreeMap<String, String> = parts
        .uri
        .query()
        .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    state.seen.lock().expect("mock lock").push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query,
        api_key: parts.headers.get("api-key").and_then(|v| v.to_str().ok()).map(String::from),
        body: body.to_vec(),
    });
    let (status, payload) = state.reply.lock().expect("mock lock").clone();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .expect("mock response")
}

// ── HTTP plumbing ────────────────────────────────────────────────────────────

struct Call {
    method: Method,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Call {
    fn new(method: Method, path: &str) -> Self {
        Self { method, path: path.to_string(), headers: Vec::new(), body: Vec::new() }
    }

    fn get(path: &str) -> Self {
        Self::new(Method::GET, path)
    }

    fn post(path: &str) -> Self {
        Self::new(Method::POST, path)
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn headers(mut self, all: &[(String, String)]) -> Self {
        self.headers.extend(all.iter().cloned());
        self
    }

    fn json(self, value: &Value) -> Self {
        let mut call = self.header("content-type", "application/json");
        call.body = serde_json::to_vec(value).expect("JSON body");
        call
    }

    fn form(self, pairs: &[(&str, &str)]) -> Self {
        let mut call = self.header("content-type", "application/x-www-form-urlencoded");
        call.body = query(pairs).into_bytes();
        call
    }

    fn basic_auth(self, user: &str, password: &str) -> Self {
        self.header("authorization", format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))))
    }
}

struct Resp {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    /// The JSON-RPC message of an MCP reply: the first SSE `data:` line, or
    /// the whole body when the server answered with plain JSON.
    fn rpc_payload(&self) -> Value {
        for line in self.body.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let trimmed = data.trim();
                if trimmed.is_empty() {
                    continue;
                }
                return serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("bad SSE data ({e}): {data}"));
            }
        }
        self.json()
    }
}

/// The server under test, as a child process of this test.
struct Server {
    base: String,
    child: Child,
    dir: PathBuf,
    client: Client<HttpConnector, Full<Bytes>>,
    /// The `name=value` pair of the last cookie the server set, as a browser
    /// with a single-cookie jar would keep it.
    jar: Mutex<Option<String>>,
}

impl Server {
    async fn start(hevy_url: &str, api_key: &str) -> Self {
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("find a free port");
            probe.local_addr().expect("probe address").port()
        };
        // A scratch working directory keeps a developer's own `.env` out of the run.
        let dir = std::env::temp_dir().join(format!("hevy-mcp-it-{}-{port}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch directory");
        let log = File::create(dir.join("server.log")).expect("server log");

        let mut command = Command::new(env!("CARGO_BIN_EXE_hevy-mcp"));
        command.current_dir(&dir).env_clear().stdout(Stdio::null()).stderr(Stdio::from(log));
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
            .env("HEVY_MCP_SESSION_SECRET", SECRET)
            .env("HEVY_MCP_PASSWORD", PASSWORD)
            .env("HEVY_MCP_CLIENT_ID", CLIENT_ID)
            .env("HEVY_MCP_CLIENT_SECRET", CLIENT_SECRET)
            .env("HEVY_MCP_REDIRECT_URIS", REDIRECT_URI)
            .env("HEVY_MCP_COOKIE_SECURE", "false")
            .env("HEVY_API_KEY", api_key)
            .env("HEVY_BASE_URL", hevy_url)
            .env("HEVY_MCP_BIND", format!("127.0.0.1:{port}"));
        let child = command.spawn().expect("spawn hevy-mcp");

        let mut server = Self {
            base: format!("http://127.0.0.1:{port}"),
            child,
            dir,
            client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
            jar: Mutex::new(None),
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Ok(resp) = server.try_send(Call::get("/")).await {
                if resp.status == 200 {
                    return server;
                }
            }
            if let Some(status) = server.child.try_wait().expect("poll the server") {
                let log = fs::read_to_string(server.dir.join("server.log")).unwrap_or_default();
                panic!("the server exited early ({status}):\n{log}");
            }
            assert!(Instant::now() < deadline, "the server did not start in time");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn try_send(&self, call: Call) -> Result<Resp, String> {
        let mut builder = hyper::Request::builder()
            .method(call.method)
            .uri(format!("{}{}", self.base, call.path));
        for (name, value) in &call.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(cookie) = self.jar.lock().expect("jar lock").clone() {
            builder = builder.header(header::COOKIE, cookie);
        }
        let request = builder.body(Full::new(Bytes::from(call.body))).map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| "request timed out".to_string())?
            .map_err(|e| e.to_string())?;
        let (parts, body) = response.into_parts();
        let bytes = tokio::time::timeout(TIMEOUT, body.collect())
            .await
            .map_err(|_| "reading the response timed out".to_string())?
            .map_err(|e| e.to_string())?
            .to_bytes();
        if let Some(set) = parts.headers.get(header::SET_COOKIE).and_then(|v| v.to_str().ok()) {
            *self.jar.lock().expect("jar lock") = set.split(';').next().map(str::to_string);
        }
        Ok(Resp {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }

    async fn send(&self, call: Call) -> Resp {
        let label = format!("{} {}", call.method, call.path);
        self.try_send(call).await.unwrap_or_else(|e| panic!("{label}: {e}"))
    }

    /// The value of the session cookie the browser would be holding.
    fn cookie_value(&self) -> String {
        let jar = self.jar.lock().expect("jar lock").clone().expect("the server set a cookie");
        jar.split_once('=').expect("name=value").1.to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ── MCP client ───────────────────────────────────────────────────────────────

struct Mcp<'a> {
    server: &'a Server,
    headers: Vec<(String, String)>,
    modern: bool,
}

impl<'a> Mcp<'a> {
    fn new(server: &'a Server, token: &str) -> Self {
        Self {
            server,
            headers: vec![
                ("authorization".into(), format!("Bearer {token}")),
                ("accept".into(), ACCEPT.into()),
            ],
            modern: false,
        }
    }

    fn set(&mut self, name: &str, value: &str) {
        self.headers.retain(|(n, _)| n != name);
        self.headers.push((name.to_string(), value.to_string()));
    }

    /// Open a session (legacy revision) or announce the stateless revision.
    async fn connect(&mut self, modern: bool) {
        self.modern = modern;
        self.set("mcp-protocol-version", if modern { MODERN } else { LEGACY });
        if modern {
            self.rpc("server/discover", json!({})).await;
            return;
        }
        let init = self
            .server
            .send(Call::post("/mcp/").headers(&self.headers).json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": LEGACY, "capabilities": {},
                    "clientInfo": {"name": "integration-test", "version": "1"},
                },
            })))
            .await;
        assert_eq!(init.status, 200, "{}", init.body);
        assert_eq!(init.rpc_payload()["result"]["serverInfo"]["name"], "Hevy");
        let version = init.rpc_payload()["result"]["serverInfo"]["version"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(version.starts_with("0.1.0 (Rust "), "{version}");
        let session = init.header("mcp-session-id").to_string();
        assert!(!session.is_empty(), "initialize returned no mcp-session-id");
        self.set("mcp-session-id", &session);
        let ack = self
            .server
            .send(Call::post("/mcp/").headers(&self.headers).json(&json!({
                "jsonrpc": "2.0", "method": "notifications/initialized",
            })))
            .await;
        assert_eq!(ack.status, 202, "{}", ack.body);
    }

    /// One JSON-RPC request; returns the `result`, failing on any error.
    async fn rpc(&self, method: &str, mut params: Value) -> Value {
        let mut call = Call::post("/mcp/").headers(&self.headers);
        if self.modern {
            call = call.header("mcp-method", method);
            if method == "tools/call" {
                call = call.header("mcp-name", params["name"].as_str().expect("tool name"));
            }
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientCapabilities": {},
            });
        }
        let response = self
            .server
            .send(call.json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})))
            .await;
        assert_eq!(response.status, 200, "{method}: {}", response.body);
        let mut message = response.rpc_payload();
        assert!(message.get("error").is_none(), "{method}: {message}");
        message["result"].take()
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments})).await
    }
}

/// The JSON a tool returned as its text content.
fn tool_json(result: &Value) -> Value {
    let text = result["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text content: {result}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text is not JSON ({e}): {text}"))
}

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn exercises() -> Value {
    json!([{
        "exercise_template_id": "bench", "superset_id": 1, "rest_seconds": 90,
        "notes": "Controlled", "sets": [{"weight_kg": 60, "reps": 8, "rpe": 8}],
    }])
}

fn workout() -> Value {
    json!({
        "title": "Test workout", "start_time": "2026-09-01T10:00:00Z",
        "end_time": "2026-09-01T11:00:00Z", "exercises": exercises(),
    })
}

struct Case {
    name: &'static str,
    args: Value,
    method: &'static str,
    path: &'static str,
}

fn case(name: &'static str, args: Value, method: &'static str, path: &'static str) -> Case {
    Case { name, args, method, path }
}

fn cases() -> Vec<Case> {
    let mut update_workout = workout();
    update_workout["workout_id"] = json!("w1");
    update_workout["is_private"] = json!(false);
    update_workout["description"] = json!("Test description");
    vec![
        case("workout_count", json!({}), "GET", "/v1/workouts/count"),
        case("list_workouts", json!({"page_size": 50}), "GET", "/v1/workouts"),
        case("get_workout", json!({"workout_id": "w1"}), "GET", "/v1/workouts/w1"),
        case("list_routines", json!({}), "GET", "/v1/routines"),
        case("get_routine", json!({"routine_id": "r1"}), "GET", "/v1/routines/r1"),
        case("list_routine_folders", json!({}), "GET", "/v1/routine_folders"),
        case("get_routine_folder", json!({"folder_id": 42}), "GET", "/v1/routine_folders/42"),
        case("get_user_info", json!({}), "GET", "/v1/user/info"),
        case("search_exercise_templates", json!({"query": "bench"}), "GET", "/v1/exercise_templates"),
        case("get_exercise_template", json!({"template_id": "bench"}), "GET", "/v1/exercise_templates/bench"),
        // Three entries: the date window is optional, and params it was not
        // given must be omitted from the query string entirely.
        case("get_exercise_history", json!({"exercise_template_id": "bench"}), "GET", "/v1/exercise_history/bench"),
        case(
            "get_exercise_history",
            json!({"exercise_template_id": "bench", "start_date": "2026-01-01T00:00:00Z"}),
            "GET",
            "/v1/exercise_history/bench",
        ),
        case(
            "get_exercise_history",
            json!({"exercise_template_id": "bench", "start_date": "2026-01-01T00:00:00Z", "end_date": "2026-06-01T00:00:00Z"}),
            "GET",
            "/v1/exercise_history/bench",
        ),
        case("create_routine", json!({"title": "Test routine", "exercises": exercises()}), "POST", "/v1/routines"),
        case(
            "update_routine",
            json!({"routine_id": "r1", "title": "Test routine", "notes": "Test notes", "exercises": exercises()}),
            "PUT",
            "/v1/routines/r1",
        ),
        case("create_routine_folder", json!({"title": "Test folder"}), "POST", "/v1/routine_folders"),
        case("create_workout", workout(), "POST", "/v1/workouts"),
        case("update_workout", update_workout, "PUT", "/v1/workouts/w1"),
        case(
            "create_exercise_template",
            json!({"title": "Test exercise", "exercise_type": "weight_reps", "equipment_category": "barbell", "muscle_group": "chest"}),
            "POST",
            "/v1/exercise_templates",
        ),
    ]
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_and_auth_for_both_protocols() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;

    assert_eq!(server.send(Call::get("/")).await.json()["status"], "ok");

    let wrong_client = mint_token("other");
    // A browser session cookie must not pass as an access token either.
    let session = session_serializer().dumps(&json!("owner"));
    for version in [LEGACY, MODERN] {
        for token in [None, Some("forged".to_string()), Some(session.clone()), Some(wrong_client.clone())] {
            let mut call = Call::post("/mcp/")
                .header("accept", ACCEPT)
                .header("mcp-protocol-version", version)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
            if let Some(token) = &token {
                call = call.header("authorization", format!("Bearer {token}"));
            }
            let response = server.send(call).await;
            assert_eq!(response.status, 401, "{version} {token:?}");
            assert_eq!(response.header("www-authenticate"), "Bearer", "{version} {token:?}");
        }
    }

    // A valid token in the query string is not accepted: only the Bearer header is.
    let valid = mint_token(CLIENT_ID);
    let response = server.send(Call::post(&format!("/mcp/?token={valid}")).json(&json!({}))).await;
    assert_eq!(response.status, 401);
}

async fn exercise_tools(modern: bool) {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(modern).await;

    let listed = mcp.rpc("tools/list", json!({})).await;
    let tools = listed["tools"].as_array().expect("tools array");
    let cases = cases();
    let names: BTreeSet<&str> = tools.iter().map(|t| t["name"].as_str().expect("tool name")).collect();
    let expected: BTreeSet<&str> = cases.iter().map(|c| c.name).collect();
    assert_eq!(names, expected);
    assert_eq!(names.len(), 17);

    let create_workout = tools.iter().find(|t| t["name"] == "create_workout").expect("create_workout");
    let schema = &create_workout["inputSchema"];
    assert!(schema["required"].as_array().expect("required").contains(&json!("exercises")), "{schema}");
    let nested = &schema["properties"]["exercises"]["items"]["required"];
    assert!(nested.as_array().expect("nested required").contains(&json!("sets")), "{schema}");

    for case in &cases {
        let label = format!("{} {} (modern={modern})", case.name, case.args);
        let upstream = match case.name {
            "workout_count" => json!({"workout_count": 12}),
            "search_exercise_templates" => json!({"page_count": 1, "exercise_templates": [{
                "id": "bench", "title": "Bench press", "type": "weight_reps",
                "primary_muscle_group": "chest", "equipment": "barbell", "is_custom": false,
            }]}),
            name => json!({"ok": name}),
        };
        hevy.take();
        hevy.reply_json(200, &upstream);

        let result = mcp.call_tool(case.name, case.args.clone()).await;
        let seen = hevy.take();

        assert!(!result["isError"].as_bool().unwrap_or(false), "{label}: {result}");
        let expected = if case.name == "search_exercise_templates" {
            upstream["exercise_templates"].clone()
        } else {
            upstream.clone()
        };
        assert_eq!(tool_json(&result), expected, "{label}");

        assert_eq!(seen.len(), 1, "{label}");
        let request = &seen[0];
        assert_eq!((request.method.as_str(), request.path.as_str()), (case.method, case.path), "{label}");
        assert_eq!(request.api_key.as_deref(), Some(API_KEY), "{label}");
        if case.name == "list_workouts" {
            assert_eq!(request.query.get("pageSize").map(String::as_str), Some("10"), "{label}");
        }
        if case.name == "get_exercise_history" {
            let dates: BTreeMap<String, String> = case
                .args
                .as_object()
                .expect("arguments")
                .iter()
                .filter(|(key, _)| key.ends_with("_date"))
                .map(|(key, value)| (key.clone(), value.as_str().expect("date").to_string()))
                .collect();
            assert_eq!(request.query, dates, "{label}");
        }
        if case.args.get("exercises").is_some() {
            let kind = if case.name.contains("routine") { "routine" } else { "workout" };
            let sent: Value = serde_json::from_slice(&request.body).expect("request body is JSON");
            let exercise = &sent[kind]["exercises"][0];
            let set = &exercise["sets"][0];
            assert_eq!(exercise["exercise_template_id"], "bench", "{label}");
            assert_eq!(exercise["superset_id"].as_f64(), Some(1.0), "{label}");
            assert_eq!(set["weight_kg"].as_f64(), Some(60.0), "{label}");
            assert_eq!(set["type"], "normal", "{label}");
            if kind == "routine" {
                assert_eq!(exercise["rest_seconds"].as_f64(), Some(90.0), "{label}");
                assert!(set.get("rpe").is_none(), "{label}");
            } else {
                assert_eq!(set["rpe"].as_f64(), Some(8.0), "{label}");
                assert!(exercise.get("rest_seconds").is_none(), "{label}");
            }
        }
    }
}

#[tokio::test]
async fn all_tools_legacy() {
    exercise_tools(false).await;
}

#[tokio::test]
async fn all_tools_modern() {
    exercise_tools(true).await;
}

#[tokio::test]
async fn validation_and_upstream_errors() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    hevy.reply_text(429, "Try later");
    let mut no_template = workout();
    no_template["exercises"] = json!([{"sets": []}]);
    for args in [json!({}), no_template] {
        let result = mcp.call_tool("create_workout", args.clone()).await;
        assert_eq!(result["isError"], true, "{args}");
    }
    assert!(hevy.take().is_empty(), "invalid arguments must not reach Hevy");

    let result = mcp.call_tool("workout_count", json!({})).await;
    assert_eq!(tool_json(&result), json!({"error": "Hevy API returned 429: Try later"}));

    // A server without an API key still boots and says so when a tool needs it.
    let unconfigured = Server::start(&hevy.url, "").await;
    let mut mcp = Mcp::new(&unconfigured, &mint_token(CLIENT_ID));
    mcp.connect(false).await;
    let result = mcp.call_tool("workout_count", json!({})).await;
    let message = tool_json(&result)["error"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("HEVY_API_KEY is missing"), "{message}");
}

#[tokio::test]
async fn oauth_login_pkce_and_single_use_code() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;

    let metadata = server
        .send(Call::get("/.well-known/oauth-authorization-server").header("host", "check"))
        .await
        .json();
    assert_eq!(metadata["token_endpoint"], "http://check/oauth/token");

    let verifier = "a".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let params = [
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("state", "test-state"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    let mut authorize_params = vec![("response_type", "code")];
    authorize_params.extend(params);
    let authorize = format!("/oauth/authorize?{}", query(&authorize_params));

    // Not signed in yet: sent to the login page.
    let response = server.send(Call::get(&authorize)).await;
    assert_eq!(response.status, 302);
    assert!(response.header("location").starts_with("/login?"), "{}", response.header("location"));
    assert_eq!(server.send(Call::get("/login")).await.status, 200);
    assert_eq!(server.send(Call::post("/login").form(&[("password", "wrong")])).await.status, 401);
    assert_eq!(server.send(Call::post("/login").form(&[("password", PASSWORD)])).await.status, 303);

    // Signed in: the consent page, then approval hands back a code.
    assert_eq!(server.send(Call::get(&authorize)).await.status, 200);
    let approved = server.send(Call::post("/oauth/authorize/approve").form(&params)).await;
    assert_eq!(approved.status, 302);
    let (_, redirect_query) = approved.header("location").split_once('?').expect("redirect carries a query");
    let redirect: HashMap<String, String> = form_urlencoded::parse(redirect_query.as_bytes()).into_owned().collect();
    assert_eq!(redirect["state"], "test-state");

    let exchange = [
        ("grant_type", "authorization_code"),
        ("code", redirect["code"].as_str()),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", verifier.as_str()),
    ];
    let token = server.send(Call::post("/oauth/token").form(&exchange)).await;
    assert_eq!(token.status, 200, "{}", token.body);
    let access_token = token.json()["access_token"].as_str().expect("access_token").to_string();

    // The issued token opens an MCP session.
    let mut mcp = Mcp::new(&server, &access_token);
    mcp.connect(false).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    assert_eq!(listed["tools"].as_array().map(Vec::len), Some(17));

    // A code works once.
    let replay = server.send(Call::post("/oauth/token").form(&exchange)).await;
    assert_eq!(replay.status, 400);
    assert_eq!(replay.json()["error"], "invalid_grant");
}

/// What the server issues is the signed format existing Claude connections
/// were created with, including client authentication by HTTP Basic.
#[tokio::test]
async fn issued_credentials_use_the_signed_formats() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;

    server.send(Call::post("/login").form(&[("password", PASSWORD)])).await;
    assert_eq!(session_serializer().loads(&server.cookie_value(), 60), Some(json!("owner")));

    let consent = server
        .send(Call::get(&format!(
            "/oauth/authorize?{}",
            query(&[("response_type", "code"), ("client_id", CLIENT_ID), ("redirect_uri", REDIRECT_URI)])
        )))
        .await;
    assert_eq!(consent.status, 200);
    let approved = server
        .send(Call::post("/oauth/authorize/approve").form(&[("client_id", CLIENT_ID), ("redirect_uri", REDIRECT_URI)]))
        .await;
    assert_eq!(approved.status, 302);
    let (_, redirect_query) = approved.header("location").split_once('?').expect("redirect carries a query");
    let redirect: HashMap<String, String> = form_urlencoded::parse(redirect_query.as_bytes()).into_owned().collect();

    let response = server
        .send(
            Call::post("/oauth/token")
                .basic_auth(CLIENT_ID, CLIENT_SECRET)
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", redirect["code"].as_str()),
                    ("redirect_uri", REDIRECT_URI),
                ]),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = response.json();
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 31_536_000);
    assert_eq!(
        token_serializer().loads(body["access_token"].as_str().expect("access_token"), 60),
        Some(json!({"client_id": CLIENT_ID, "type": "mcp_access_token"}))
    );
}

#[tokio::test]
async fn acceptance_handshake_revision_initialize_list_and_call() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    let listed = mcp.rpc("tools/list", json!({})).await;
    assert_eq!(listed["tools"].as_array().map(Vec::len), Some(17));

    hevy.reply_json(200, &json!({"workout_count": 42}));
    let result = mcp.call_tool("workout_count", json!({})).await;
    assert_eq!(tool_json(&result), json!({"workout_count": 42}));
}

#[tokio::test]
async fn acceptance_revision_2026_07_28_discover_cache_scope_and_subscriptions_listen() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(true).await;

    // 1. server/discover succeeds
    let discover = mcp.rpc("server/discover", json!({})).await;
    assert!(discover.get("capabilities").is_some(), "discover capabilities: {discover}");
    assert!(discover.get("supportedVersions").is_some(), "discover supportedVersions: {discover}");

    // 2. tools/list carries cacheScope of "public" or "private"
    let tools = mcp.rpc("tools/list", json!({})).await;
    let scope = tools["cacheScope"].as_str().unwrap_or_default();
    assert!(
        scope == "public" || scope == "private",
        "tools/list cacheScope should be 'public' or 'private', got: {tools}"
    );

    // 3. subscriptions/listen answers 200 text/event-stream, and its first event
    // (notifications/subscriptions/acknowledged) arrives within 2 s while the stream stays open.
    let token = mint_token(CLIENT_ID);
    let builder = hyper::Request::builder()
        .method(Method::POST)
        .uri(format!("{}/mcp/", server.base))
        .header("authorization", format!("Bearer {token}"))
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", "subscriptions/listen")
        .header("content-type", "application/json");

    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": "sub-1",
        "method": "subscriptions/listen",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientCapabilities": {},
            },
            "notifications": {
                "toolsListChanged": true,
            },
        },
    }))
    .expect("body JSON");

    let req = builder.body(Full::new(Bytes::from(body))).expect("valid request");
    let resp = server.client.request(req).await.expect("send request");
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream, got {content_type}"
    );

    let mut incoming = resp.into_body();
    let mut buffer = String::new();
    let start = Instant::now();
    let mut ack_received = false;

    while start.elapsed() < Duration::from_secs(2) {
        let frame_result = tokio::time::timeout(Duration::from_millis(500), incoming.frame()).await;
        if let Ok(Some(Ok(frame))) = frame_result {
            if let Some(data) = frame.data_ref() {
                buffer.push_str(&String::from_utf8_lossy(data));
                if buffer.contains("notifications/subscriptions/acknowledged") {
                    ack_received = true;
                    break;
                }
            }
        }
    }
    assert!(
        ack_received,
        "did not receive notifications/subscriptions/acknowledged within 2s; buffer: {buffer}"
    );

    // Verify stream stays open: reading another frame with a short timeout does not return EOF (None)
    let next_frame = tokio::time::timeout(Duration::from_millis(200), incoming.frame()).await;
    assert!(
        matches!(next_frame, Err(_) | Ok(Some(_))),
        "stream should stay open, but closed early: {next_frame:?}"
    );
}

#[tokio::test]
async fn test_update_workout_safe_defaults() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    // 1. Initial workout on Hevy with is_private = true and non-empty description
    let initial_workout = json!({
        "workout": {
            "id": "w_safe",
            "title": "Private Leg Day",
            "description": "Heavy squats, secret notes",
            "is_private": true,
            "start_time": "2026-09-01T10:00:00Z",
            "end_time": "2026-09-01T11:00:00Z",
            "exercises": exercises()
        }
    });
    hevy.reply_json(200, &initial_workout);

    // 2. Read the workout via get_workout
    let read_res = mcp.call_tool("get_workout", json!({"workout_id": "w_safe"})).await;
    assert!(!read_res["isError"].as_bool().unwrap_or(false));
    let seen_read = hevy.take();
    assert_eq!(seen_read.len(), 1);
    assert_eq!(seen_read[0].path, "/v1/workouts/w_safe");

    // 3. Update workout WITHOUT providing is_private or description. The mock
    // answers every request with one body, so it keeps serving the private
    // workout: that is what the connector's GET must see before its PUT.
    hevy.reply_json(200, &initial_workout);
    let mut update_args = workout();
    update_args["workout_id"] = json!("w_safe");
    update_args["title"] = json!("Updated Leg Day");
    assert!(update_args.get("is_private").is_none());
    assert!(update_args.get("description").is_none());

    let update_res = mcp.call_tool("update_workout", update_args).await;
    assert!(!update_res["isError"].as_bool().unwrap_or(false));

    // The connector must read the workout first, then issue PUT with preserved fields
    let seen_update = hevy.take();
    assert_eq!(seen_update.len(), 2, "Expected GET then PUT");
    assert_eq!(seen_update[0].method, "GET");
    assert_eq!(seen_update[0].path, "/v1/workouts/w_safe");
    assert_eq!(seen_update[1].method, "PUT");
    assert_eq!(seen_update[1].path, "/v1/workouts/w_safe");

    let put_body: Value = serde_json::from_slice(&seen_update[1].body).expect("valid PUT body JSON");
    let sent_workout = &put_body["workout"];
    assert_eq!(
        sent_workout["is_private"],
        json!(true),
        "is_private must be preserved from existing workout"
    );
    assert_eq!(
        sent_workout["description"],
        json!("Heavy squats, secret notes"),
        "description must be preserved from existing workout"
    );
    assert_eq!(sent_workout["title"], json!("Updated Leg Day"));
}

#[tokio::test]
async fn test_search_exercise_templates_russian_and_empty() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    // The mock answers every path with this one body: the template library for
    // /v1/exercise_templates, and a workout logged in Russian for /v1/workouts.
    let templates = json!({
        "page_count": 1,
        "workouts": [{
            "id": "w1",
            "exercises": [
                {"exercise_template_id": "7EB3F7C3", "title": "Жим от груди (Тренажёр)"},
                {"exercise_template_id": "other_1", "title": "Bicep Curl"}
            ]
        }],
        "exercise_templates": [
            {
                "id": "7EB3F7C3",
                "title": "Chest Press (Machine)",
                "type": "weight_reps",
                "primary_muscle_group": "chest",
                "equipment": "machine",
                "is_custom": false
            },
            {
                "id": "other_1",
                "title": "Bicep Curl",
                "type": "weight_reps",
                "primary_muscle_group": "biceps",
                "equipment": "dumbbell",
                "is_custom": false
            }
        ]
    });
    hevy.reply_json(200, &templates);

    // 1. Search with Russian term "жим"
    let res = mcp.call_tool("search_exercise_templates", json!({"query": "жим"})).await;
    assert!(!res["isError"].as_bool().unwrap_or(false));
    let items = tool_json(&res);
    let items_arr = items.as_array().expect("array of results");
    assert_eq!(items_arr.len(), 1);
    assert_eq!(items_arr[0]["id"], "7EB3F7C3");
    assert_eq!(items_arr[0]["title"], "Chest Press (Machine)");
    assert_eq!(items_arr[0]["title_ru"], "Жим от груди (Тренажёр)");
    assert!(hevy.take().iter().any(|s| s.path == "/v1/workouts"), "Russian titles come from logged workouts");

    // 1b. ё and е are the same letter for the search
    let res = mcp.call_tool("search_exercise_templates", json!({"query": "тренажер"})).await;
    assert_eq!(tool_json(&res).as_array().expect("array").len(), 1);

    // 1c. a Russian word matches only exercises logged under it, not every "press"
    let res = mcp.call_tool("search_exercise_templates", json!({"query": "присед"})).await;
    assert_eq!(tool_json(&res), json!([]));

    // 2. Search for non-matching term returns empty array [] (not error object)
    hevy.reply_json(200, &templates);
    let empty_res = mcp.call_tool("search_exercise_templates", json!({"query": "nonexistent_query"})).await;
    assert!(!empty_res["isError"].as_bool().unwrap_or(false));
    let empty_items = tool_json(&empty_res);
    assert_eq!(empty_items, json!([]));

    // 3. get_exercise_template returns title_ru and strips thumbnail_url
    let single_template = json!({
        "id": "7EB3F7C3",
        "title": "Chest Press (Machine)",
        "type": "weight_reps",
        "primary_muscle_group": "chest",
        "equipment": "machine",
        "thumbnail_url": "https://pump-app.s3.eu-west-2.amazonaws.com/exercise-thumbnails/05771201.jpg"
    });
    hevy.reply_json(200, &single_template);
    let tmpl_res = mcp.call_tool("get_exercise_template", json!({"template_id": "7EB3F7C3"})).await;
    assert!(!tmpl_res["isError"].as_bool().unwrap_or(false));
    let tmpl_json = tool_json(&tmpl_res);
    assert_eq!(tmpl_json["title_ru"], "Жим от груди (Тренажёр)");
    assert!(tmpl_json.get("thumbnail_url").is_none(), "thumbnail_url should be removed");

    // 4. an upstream failure is an MCP error, not "nothing found"
    hevy.reply_text(401, "{\"error\": \"Invalid api key\"}");
    let failed = mcp.call_tool("search_exercise_templates", json!({"query": "bench"})).await;
    assert!(failed["isError"].as_bool().unwrap_or(false), "{failed}");
}

#[tokio::test]
async fn test_routine_folders_normalizes_routines_key() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    // Upstream Hevy returns 'routines' key instead of 'routine_folders'
    hevy.reply_json(
        200,
        &json!({
            "page": 1,
            "page_count": 1,
            "routines": [{"id": 10, "title": "Push / Pull / Legs"}]
        }),
    );

    let res = mcp.call_tool("list_routine_folders", json!({})).await;
    assert!(!res["isError"].as_bool().unwrap_or(false));
    let payload = tool_json(&res);
    assert!(payload.get("routines").is_none(), "Key 'routines' must be normalized");
    assert_eq!(
        payload["routine_folders"],
        json!([{"id": 10, "title": "Push / Pull / Legs"}])
    );
}

#[tokio::test]
async fn test_error_handling_and_404_conventions() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    // 1. Paginated out-of-bounds page returns empty list with page_count (not error)
    hevy.reply_text(404, r#"{"error": "Page not found"}"#);
    let list_res = mcp.call_tool("list_workouts", json!({"page": 99})).await;
    assert!(!list_res["isError"].as_bool().unwrap_or(false));
    let list_payload = tool_json(&list_res);
    assert_eq!(list_payload["page"], 99);
    assert_eq!(list_payload["page_count"], 0);
    assert_eq!(list_payload["workouts"], json!([]));

    // 2. Entity lookup 404 returns MCP isError: true with clean message
    hevy.reply_text(404, "Workout not found");
    let get_res = mcp.call_tool("get_workout", json!({"workout_id": "00000000-0000"})).await;
    assert_eq!(get_res["isError"], true);
    let err_payload = tool_json(&get_res);
    assert_eq!(err_payload["error"], "workout 00000000-0000 not found");
}

#[tokio::test]
async fn test_exercise_history_grouping_and_null_stripping() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    let flat_history = json!({
        "exercise_history": [
            {
                "workout_id": "w1",
                "workout_title": "Chest Day",
                "workout_start_time": "2026-08-01T10:00:00+00:00",
                "workout_end_time": "2026-08-01T11:00:00Z",
                "exercise_template_id": "7EB3F7C3",
                "weight_kg": 60.0,
                "reps": 10,
                "set_type": "normal",
                "distance_meters": null,
                "duration_seconds": null,
                "rpe": null,
                "custom_metric": null
            },
            {
                "workout_id": "w1",
                "workout_title": "Chest Day",
                "workout_start_time": "2026-08-01T10:00:00+00:00",
                "workout_end_time": "2026-08-01T11:00:00Z",
                "exercise_template_id": "7EB3F7C3",
                "weight_kg": 65.0,
                "reps": 8,
                "set_type": "normal",
                "distance_meters": null,
                "duration_seconds": null,
                "rpe": 8.5,
                "custom_metric": null
            }
        ]
    });

    // 1. Regular grouping (sets have set_index, null fields are stripped)
    hevy.reply_json(200, &flat_history);
    let res = mcp.call_tool("get_exercise_history", json!({"exercise_template_id": "7EB3F7C3"})).await;
    assert!(!res["isError"].as_bool().unwrap_or(false));
    let payload = tool_json(&res);
    assert_eq!(payload["common"]["exercise_template_id"], "7EB3F7C3");
    let workouts = payload["workouts"].as_array().expect("workouts array");
    assert_eq!(workouts.len(), 1);
    let w1 = &workouts[0];
    assert_eq!(w1["workout_id"], "w1");
    let sets = w1["sets"].as_array().expect("sets array");
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0]["set_index"], 1);
    assert_eq!(sets[0]["weight_kg"], 60.0);
    assert!(sets[0].get("distance_meters").is_none(), "null fields must be stripped");
    assert_eq!(sets[1]["set_index"], 2);
    assert_eq!(sets[1]["rpe"], 8.5);

    // 2. Summary mode (top_set, volume_kg, estimated_1rm calculated)
    hevy.reply_json(200, &flat_history);
    let sum_res = mcp
        .call_tool(
            "get_exercise_history",
            json!({"exercise_template_id": "7EB3F7C3", "summary": true}),
        )
        .await;
    assert!(!sum_res["isError"].as_bool().unwrap_or(false));
    let sum_payload = tool_json(&sum_res);
    let sum_w1 = &sum_payload["workouts"][0];
    assert_eq!(sum_w1["top_set"], json!({"weight_kg": 65.0, "reps": 8}));
    assert_eq!(sum_w1["volume_kg"], json!(1120.0));
    assert_eq!(sum_w1["estimated_1rm"], json!(82.3));
}

#[tokio::test]
async fn test_list_workouts_and_routines_summary_mode() {
    let hevy = Hevy::start().await;
    let server = Server::start(&hevy.url, API_KEY).await;
    let mut mcp = Mcp::new(&server, &mint_token(CLIENT_ID));
    mcp.connect(false).await;

    let raw_workouts = json!({
        "page": 1,
        "page_count": 1,
        "workouts": [
            {
                "id": "w1",
                "title": "Chest & Triceps",
                "start_time": "2026-08-04T13:33:10+00:00",
                "end_time": "2026-08-04T14:28:51.015Z",
                "exercises": [
                    {
                        "title": "Chest Press (Machine)",
                        "sets": [
                            {"weight_kg": 60.0, "reps": 10, "distance_meters": null},
                            {"weight_kg": 65.0, "reps": 8, "distance_meters": null}
                        ]
                    }
                ]
            }
        ]
    });

    // 1. Default summary mode returns compact workout representation
    hevy.reply_json(200, &raw_workouts);
    let res = mcp.call_tool("list_workouts", json!({})).await;
    assert!(!res["isError"].as_bool().unwrap_or(false));
    let payload = tool_json(&res);
    let w1 = &payload["workouts"][0];
    assert_eq!(w1["id"], "w1");
    assert_eq!(w1["title"], "Chest & Triceps");
    assert_eq!(w1["start_time"], "2026-08-04T13:33:10Z");
    assert_eq!(w1["start_time_local"], "2026-08-04T16:33:10+03:00");
    assert_eq!(w1["duration_min"], 55);
    assert_eq!(w1["exercises_count"], 1);
    assert_eq!(w1["volume_kg"], 1120.0);
    assert_eq!(w1["exercises"], json!(["Chest Press (Machine)"]));
    assert!(w1.get("sets").is_none(), "summary mode omits detailed sets");

    // 2. Full mode returns complete workout tree
    hevy.reply_json(200, &raw_workouts);
    let full_res = mcp.call_tool("list_workouts", json!({"detail": "full"})).await;
    assert!(!full_res["isError"].as_bool().unwrap_or(false));
    let full_payload = tool_json(&full_res);
    let full_w1 = &full_payload["workouts"][0];
    assert!(full_w1.get("exercises").is_some());
    assert!(full_w1["exercises"][0].get("sets").is_some());

    // 3. Routines summary mode
    let raw_routines = json!({
        "page": 1,
        "page_count": 1,
        "routines": [
            {
                "id": "r1",
                "title": "Push Day",
                "folder_id": 1,
                "updated_at": "2026-08-04T10:00:00.000Z",
                "exercises": [
                    {
                        "title": "Bench Press",
                        "sets": [{"weight_kg": 80.0, "reps": 5}]
                    }
                ]
            }
        ]
    });
    hevy.reply_json(200, &raw_routines);
    let r_res = mcp.call_tool("list_routines", json!({})).await;
    assert!(!r_res["isError"].as_bool().unwrap_or(false));
    let r_payload = tool_json(&r_res);
    let r1 = &r_payload["routines"][0];
    assert_eq!(r1["id"], "r1");
    assert_eq!(r1["title"], "Push Day");
    assert_eq!(r1["exercises"], json!(["Bench Press"]));
    assert_eq!(r1["updated_at"], "2026-08-04T10:00:00Z");
}
