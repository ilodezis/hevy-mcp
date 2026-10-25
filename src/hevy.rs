//! Hevy REST client over the `/v1` API, authenticated with the personal
//! `api-key` header. Only an actual request needs the key, so the server
//! boots and answers health checks before Hevy is configured.
//!
//! Docs: https://api.hevyapp.com/docs/
//!   * List endpoints are paginated: `{"page", "page_count", <collection>: [...]}`.
//!   * `pageSize` caps at 10 for workouts/routines, 100 for exercise_templates.
//!   * The public API exposes no DELETE endpoints.

use std::{fmt, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, header};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_json::{Map, Value, json};

use crate::urlenc::build_url;

const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = concat!("hevy-mcp/", env!("CARGO_PKG_VERSION"));

pub enum HevyError {
    /// A request was attempted without `HEVY_API_KEY`.
    NotConfigured,
    /// Non-2xx response; the message is the first 400 characters of the body.
    Api { status: u16, message: String },
    /// Transport failure, timeout, or an unparseable response.
    Unexpected(String),
}

impl fmt::Display for HevyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => {
                f.write_str("Hevy is not configured — HEVY_API_KEY is missing on the server.")
            }
            Self::Api { status, message } => write!(f, "Hevy API returned {status}: {message}"),
            Self::Unexpected(message) => write!(f, "Unexpected error: {message}"),
        }
    }
}

pub type HevyResult<T = Value> = Result<T, HevyError>;

#[derive(Clone)]
pub struct HevyClient {
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    base_url: String,
    api_key: String,
}

impl HevyClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        let mut tcp = HttpConnector::new();
        tcp.set_connect_timeout(Some(TIMEOUT));
        tcp.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .expect("ring provider supports the default TLS versions")
            .https_or_http()
            .enable_http1()
            .wrap_connector(tcp);
        let http = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(2)
            .build(https);
        Self { http, base_url, api_key }
    }

    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        body: Option<Value>,
    ) -> HevyResult {
        if self.api_key.is_empty() {
            return Err(HevyError::NotConfigured);
        }
        let url = build_url(&self.base_url, path, params);
        let body = match body {
            Some(value) => Bytes::from(encode_json(&value)?),
            None => Bytes::new(),
        };
        let mut req = Request::builder()
            .method(method)
            .uri(&url)
            .header("api-key", &self.api_key)
            .header(header::ACCEPT, "application/json")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::USER_AGENT, USER_AGENT);
        if !body.is_empty() {
            req = req.header(header::CONTENT_LENGTH, body.len());
        }
        let req = req
            .body(Full::new(body))
            .map_err(|e| HevyError::Unexpected(format!("Invalid URL {url:?}: {e}")))?;

        let resp = tokio::time::timeout(TIMEOUT, self.http.request(req))
            .await
            .map_err(|_| HevyError::Unexpected("timed out waiting for Hevy".into()))?
            .map_err(|e| HevyError::Unexpected(error_chain(&e)))?;
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        let mut content = Vec::new();
        loop {
            let frame = tokio::time::timeout(TIMEOUT, body.frame())
                .await
                .map_err(|_| HevyError::Unexpected("timed out reading the Hevy response".into()))?;
            match frame {
                None => break,
                Some(Err(e)) => return Err(HevyError::Unexpected(error_chain(&e))),
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        content.extend_from_slice(data);
                    }
                }
            }
        }

        if status >= 400 {
            let text = String::from_utf8_lossy(&content);
            return Err(HevyError::Api { status, message: text.chars().take(400).collect() });
        }
        // 201/204 with an empty body → {} rather than choke on parsing.
        if content.is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        serde_json::from_slice(&content).map_err(|e| HevyError::Unexpected(e.to_string()))
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> HevyResult {
        self.request(Method::GET, path, params, None).await
    }

    fn page_params(page: i64, page_size: i64, max: i64) -> Vec<(&'static str, String)> {
        vec![("page", page.to_string()), ("pageSize", page_size.clamp(1, max).to_string())]
    }

    // ── Workouts ─────────────────────────────────────────────────────────────
    pub async fn workout_count(&self) -> HevyResult<Value> {
        let data = self.get("/v1/workouts/count", &[]).await?;
        Ok(match data.get("workout_count") {
            None => json!(0),
            Some(value) => py_int(value)?,
        })
    }

    pub async fn get_workouts(&self, page: i64, page_size: i64) -> HevyResult {
        self.get("/v1/workouts", &Self::page_params(page, page_size, 10)).await
    }

    pub async fn get_workout(&self, id: &str) -> HevyResult {
        self.get(&format!("/v1/workouts/{id}"), &[]).await
    }

    pub async fn create_workout(&self, workout: Value) -> HevyResult {
        self.request(Method::POST, "/v1/workouts", &[], Some(json!({"workout": workout}))).await
    }

    pub async fn update_workout(&self, id: &str, workout: Value) -> HevyResult {
        let path = format!("/v1/workouts/{id}");
        self.request(Method::PUT, &path, &[], Some(json!({"workout": workout}))).await
    }

    // ── Routines ─────────────────────────────────────────────────────────────
    pub async fn get_routines(&self, page: i64, page_size: i64) -> HevyResult {
        self.get("/v1/routines", &Self::page_params(page, page_size, 10)).await
    }

    pub async fn get_routine(&self, id: &str) -> HevyResult {
        self.get(&format!("/v1/routines/{id}"), &[]).await
    }

    pub async fn create_routine(&self, routine: Value) -> HevyResult {
        self.request(Method::POST, "/v1/routines", &[], Some(json!({"routine": routine}))).await
    }

    pub async fn update_routine(&self, id: &str, routine: Value) -> HevyResult {
        let path = format!("/v1/routines/{id}");
        self.request(Method::PUT, &path, &[], Some(json!({"routine": routine}))).await
    }

    // ── Routine folders ──────────────────────────────────────────────────────
    pub async fn get_routine_folders(&self, page: i64, page_size: i64) -> HevyResult {
        self.get("/v1/routine_folders", &Self::page_params(page, page_size, 10)).await
    }

    pub async fn get_routine_folder(&self, id: i64) -> HevyResult {
        self.get(&format!("/v1/routine_folders/{id}"), &[]).await
    }

    pub async fn create_routine_folder(&self, title: &str) -> HevyResult {
        let body = json!({"routine_folder": {"title": title}});
        self.request(Method::POST, "/v1/routine_folders", &[], Some(body)).await
    }

    // ── Exercise templates ───────────────────────────────────────────────────
    pub async fn get_exercise_templates(&self, page: i64, page_size: i64) -> HevyResult {
        self.get("/v1/exercise_templates", &Self::page_params(page, page_size, 100)).await
    }

    pub async fn get_exercise_template(&self, id: &str) -> HevyResult {
        self.get(&format!("/v1/exercise_templates/{id}"), &[]).await
    }

    pub async fn create_exercise_template(&self, exercise: Value) -> HevyResult {
        let body = json!({"exercise": exercise});
        self.request(Method::POST, "/v1/exercise_templates", &[], Some(body)).await
    }

    // ── Exercise history ─────────────────────────────────────────────────────
    /// Every set ever performed for one exercise, flat. Unpaginated — the date
    /// window is the only way to bound the response size, so empty bounds are
    /// left out of the query string entirely.
    pub async fn get_exercise_history(
        &self,
        id: &str,
        start_date: Option<&str>,
        end_date: Option<&str>,
    ) -> HevyResult {
        let params: Vec<(&str, String)> = [("start_date", start_date), ("end_date", end_date)]
            .into_iter()
            .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k, v.to_string())))
            .collect();
        self.get(&format!("/v1/exercise_history/{id}"), &params).await
    }

    // ── Account ──────────────────────────────────────────────────────────────
    pub async fn get_user_info(&self) -> HevyResult {
        self.get("/v1/user/info", &[]).await
    }
}

/// Compact JSON like httpx's `json=` encoding, which refuses NaN/Infinity.
fn encode_json(value: &Value) -> HevyResult<Vec<u8>> {
    fn non_finite(value: &Value) -> Option<f64> {
        match value {
            Value::Number(n) => n.as_f64().filter(|f| !f.is_finite()),
            Value::Array(items) => items.iter().find_map(non_finite),
            Value::Object(map) => map.values().find_map(non_finite),
            _ => None,
        }
    }
    if let Some(f) = non_finite(value) {
        let repr = if f.is_nan() { "nan" } else if f > 0.0 { "inf" } else { "-inf" };
        return Err(HevyError::Unexpected(format!(
            "Out of range float values are not JSON compliant: {repr}"
        )));
    }
    Ok(serde_json::to_vec(value).expect("JSON value"))
}

/// `int(value)` on what Hevy returned for `workout_count`.
fn py_int(value: &Value) -> HevyResult<Value> {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(value.clone()),
        Value::Number(n) => Ok(json!(n.as_f64().unwrap_or(0.0).trunc() as i64)),
        Value::Bool(b) => Ok(json!(*b as i64)),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map(|n| json!(n))
            .map_err(|_| HevyError::Unexpected(format!("invalid literal for int() with base 10: {s:?}"))),
        other => Err(HevyError::Unexpected(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            match other {
                Value::Null => "NoneType",
                Value::Array(_) => "list",
                _ => "dict",
            }
        ))),
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
