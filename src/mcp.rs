//! The Hevy tool surface exposed to Claude, on the official `rmcp` SDK.
//!
//! Conventions (a stable contract the model can rely on):
//!   * Success — the tool's normal payload (a dict, or a list of dicts).
//!   * A recoverable problem (bad id, not configured, Hevy 4xx) — an MCP
//!     error result (`isError: true`) carrying `{"error": "<human message>"}`.
//!     "Nothing found" is an empty list, not an error.
//!
//! The tool definitions in `tools.json` define the `tools/list` entries
//! (names, titles, descriptions, input and output schemas), and results are
//! shaped as a JSON text block plus `structuredContent`, with list results
//! wrapped as `{"result": …}`.
//!
//! Building a routine is a two-step dance the model should follow:
//!   1. `search_exercise_templates("bench press")` → get the `exercise_template_id`.
//!   2. `create_routine(...)` with those ids and the set scheme.
//!
//! Analysing progress follows the same two steps: `search_exercise_templates`
//! for the id, then `get_exercise_history` — not `list_workouts`, which pages
//! ten workouts at a time.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        SubscriptionFilter, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

use crate::{
    hevy::{HevyClient, HevyError, HevyResult},
    validate::{Validated, validate_call},
    version,
};

static TOOLS: LazyLock<Vec<Tool>> =
    LazyLock::new(|| serde_json::from_str(include_str!("tools.json")).expect("valid tools.json"));

fn server_info() -> Implementation {
    Implementation::new("Hevy", version::server_version())
}

#[derive(Clone)]
pub struct HevyMcp {
    hevy: Arc<HevyClient>,
}

impl HevyMcp {
    pub fn new(hevy: Arc<HevyClient>) -> Self {
        Self { hevy }
    }
}

impl ServerHandler for HevyMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        );
        info.server_info = server_info();
        info
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.get_info().capabilities))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::with_all_items(TOOLS.clone());
        result.cache_scope = Some(CacheScope::Public);
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let result = self.call(&request).await;
        Ok(result.into())
    }
}

enum Outcome {
    Dict(Value),
    List(Vec<Value>),
    Error(String),
}

fn dict(result: HevyResult) -> Outcome {
    match result {
        Ok(v) => Outcome::Dict(v),
        Err(e) => Outcome::Error(e.to_string()),
    }
}

fn text(value: &Value) -> ContentBlock {
    ContentBlock::text(serde_json::to_string(value).expect("JSON value"))
}

fn dict_result(value: Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![text(&value)]);
    result.structured_content = Some(value);
    result
}

fn list_result(items: Vec<Value>) -> CallToolResult {
    let items = Value::Array(items);
    let mut result = CallToolResult::success(vec![text(&items)]);
    result.structured_content = Some(json!({"result": items}));
    result
}

fn error_result(message: String) -> CallToolResult {
    let err_json = json!({"error": message});
    let mut result = CallToolResult::error(vec![text(&err_json)]);
    result.structured_content = Some(err_json);
    result
}

/// Read accessors over arguments the validator has already coerced.
struct Args(Map<String, Value>);

impl Args {
    fn str(&self, key: &str) -> &str {
        self.0.get(key).and_then(Value::as_str).unwrap_or_default()
    }
    fn opt_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }
    fn int(&self, key: &str) -> i64 {
        self.0.get(key).and_then(Value::as_i64).unwrap_or_default()
    }
    fn get(&self, key: &str) -> Value {
        self.0.get(key).cloned().unwrap_or(Value::Null)
    }
}

fn exercises(args: &Args, workout: bool) -> Value {
    let exercises = args.0.get("exercises").and_then(Value::as_array).cloned().unwrap_or_default();
    Value::Array(
        exercises
            .iter()
            .map(|e| {
                let field = |k: &str| e.get(k).cloned().unwrap_or(Value::Null);
                let sets: Vec<Value> = e
                    .get("sets")
                    .and_then(Value::as_array)
                    .map(|sets| {
                        sets.iter()
                            .map(|s| {
                                let f = |k: &str| s.get(k).cloned().unwrap_or(Value::Null);
                                let mut set = json!({
                                    "type": f("type"),
                                    "weight_kg": f("weight_kg"),
                                    "reps": f("reps"),
                                    "distance_meters": f("distance_meters"),
                                    "duration_seconds": f("duration_seconds"),
                                    "custom_metric": f("custom_metric"),
                                });
                                if workout {
                                    set["rpe"] = f("rpe");
                                }
                                set
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if workout {
                    json!({
                        "exercise_template_id": field("exercise_template_id"),
                        "superset_id": field("superset_id"),
                        "notes": field("notes"),
                        "sets": sets,
                    })
                } else {
                    json!({
                        "exercise_template_id": field("exercise_template_id"),
                        "superset_id": field("superset_id"),
                        "rest_seconds": field("rest_seconds"),
                        "notes": field("notes"),
                        "sets": sets,
                    })
                }
            })
            .collect(),
    )
}

impl HevyMcp {
    async fn call(&self, request: &CallToolRequestParams) -> CallToolResult {
        let name = request.name.as_ref();
        let Some(tool) = TOOLS.iter().find(|t| t.name == name) else {
            return error_result(format!("Unknown tool: '{name}'"));
        };
        let validated = match validate_call(name, &tool.input_schema, request.arguments.as_ref()) {
            Ok(validated) => validated,
            Err(report) => return error_result(report),
        };
        match self.dispatch(name, validated).await {
            Outcome::Dict(value) => dict_result(value),
            Outcome::List(items) => list_result(items),
            Outcome::Error(msg) => error_result(msg),
        }
    }

    /// Write tools carry floats into the request body; NaN/inf there fails
    /// the same way it did in Python — after the configuration check.
    fn body_guard(&self, validated: &Validated) -> HevyResult<()> {
        if !self.hevy.is_configured() {
            return Err(HevyError::NotConfigured);
        }
        match validated.non_finite {
            Some(repr) => Err(HevyError::Unexpected(format!(
                "Out of range float values are not JSON compliant: {repr}"
            ))),
            None => Ok(()),
        }
    }

    async fn dispatch(&self, name: &str, validated: Validated) -> Outcome {
        let hevy = &self.hevy;
        let guard = self.body_guard(&validated);
        let a = Args(validated.args);
        match name {
            // ── Read ────────────────────────────────────────────────────────
            "workout_count" => dict(hevy.workout_count().await.map(|n| json!({"workout_count": n}))),
            "list_workouts" => {
                match hevy.get_workouts(a.int("page"), a.int("page_size")).await {
                    Ok(val) => {
                        let detail = a.opt_str("detail").unwrap_or("summary");
                        dict(Ok(self.format_workouts_list(val, detail).await))
                    }
                    Err(HevyError::Api { status: 404, .. }) => {
                        let page = a.int("page");
                        let page_size = a.int("page_size");
                        let real_page_count = if page > 1 {
                            match hevy.get_workouts(1, page_size).await {
                                Ok(env) => env.get("page_count").and_then(Value::as_i64).unwrap_or(0),
                                Err(_) => 0,
                            }
                        } else {
                            0
                        };
                        dict(Ok(json!({
                            "page": page,
                            "page_count": real_page_count,
                            "workouts": []
                        })))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "get_workout" => {
                let id = a.str("workout_id");
                match hevy.get_workout(id).await {
                    Ok(val) => dict(Ok(clean_workout(val))),
                    Err(HevyError::Api { status: 404, .. }) => {
                        Outcome::Error(format!("workout {id} not found"))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "list_routines" => {
                match hevy.get_routines(a.int("page"), a.int("page_size")).await {
                    Ok(val) => {
                        let detail = a.opt_str("detail").unwrap_or("summary");
                        dict(Ok(format_routines_list(val, detail)))
                    }
                    Err(HevyError::Api { status: 404, .. }) => {
                        let page = a.int("page");
                        let page_size = a.int("page_size");
                        let real_page_count = if page > 1 {
                            match hevy.get_routines(1, page_size).await {
                                Ok(env) => env.get("page_count").and_then(Value::as_i64).unwrap_or(0),
                                Err(_) => 0,
                            }
                        } else {
                            0
                        };
                        dict(Ok(json!({
                            "page": page,
                            "page_count": real_page_count,
                            "routines": []
                        })))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "get_routine" => {
                let id = a.str("routine_id");
                match hevy.get_routine(id).await {
                    Ok(val) => dict(Ok(clean_workout(val))),
                    Err(HevyError::Api { status: 404, .. }) => {
                        Outcome::Error(format!("routine {id} not found"))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "list_routine_folders" => {
                match hevy.get_routine_folders(a.int("page"), a.int("page_size")).await {
                    Ok(mut envelope) => {
                        if let Some(map) = envelope.as_object_mut() {
                            if map.contains_key("routines") && !map.contains_key("routine_folders") {
                                if let Some(routines) = map.remove("routines") {
                                    map.insert("routine_folders".to_string(), routines);
                                }
                            }
                        }
                        dict(Ok(envelope))
                    }
                    Err(HevyError::Api { status: 404, .. }) => {
                        let page = a.int("page");
                        let page_size = a.int("page_size");
                        let real_page_count = if page > 1 {
                            match hevy.get_routine_folders(1, page_size).await {
                                Ok(env) => env.get("page_count").and_then(Value::as_i64).unwrap_or(0),
                                Err(_) => 0,
                            }
                        } else {
                            0
                        };
                        dict(Ok(json!({
                            "page": page,
                            "page_count": real_page_count,
                            "routine_folders": []
                        })))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "get_routine_folder" => {
                let id = a.int("folder_id");
                match hevy.get_routine_folder(id).await {
                    Ok(val) => dict(Ok(val)),
                    Err(HevyError::Api { status: 404, .. }) => {
                        Outcome::Error(format!("routine folder {id} not found"))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "get_user_info" => {
                match hevy.get_user_info().await {
                    Ok(val) => {
                        let unwrapped = val.get("data").cloned().unwrap_or(val);
                        dict(Ok(strip_nulls(unwrapped)))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "search_exercise_templates" => {
                match self.search_exercise_templates(a.str("query"), a.int("limit")).await {
                    Ok(items) => Outcome::List(items),
                    Err(e) => Outcome::Error(e.to_string()),
                }
            }
            "get_exercise_template" => {
                let id = a.str("template_id");
                match hevy.get_exercise_template(id).await {
                    Ok(mut val) => {
                        if let (Some(id_str), Some(ty)) = (val.get("id").and_then(Value::as_str), val.get("type").and_then(Value::as_str)) {
                            EXERCISE_TYPES.lock().await.insert(id_str.to_string(), ty.to_string());
                        }
                        let title_ru = cached_title_ru(id).await;
                        if let Some(map) = val.as_object_mut() {
                            map.remove("thumbnail_url");
                            if let Some(ru) = title_ru {
                                map.insert("title_ru".to_string(), json!(ru));
                            }
                        }
                        dict(Ok(val))
                    }
                    Err(HevyError::Api { status: 404, .. }) => {
                        Outcome::Error(format!("exercise template {id} not found"))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            "get_exercise_history" => {
                let id = a.str("exercise_template_id");
                let summary = a.0.get("summary").and_then(Value::as_bool).unwrap_or(false);
                let is_assisted = if summary { self.is_assisted(id).await } else { false };
                match hevy.get_exercise_history(id, a.opt_str("start_date"), a.opt_str("end_date")).await {
                    Ok(val) => {
                        let is_ass = is_assisted
                            || val.get("exercise_type").or_else(|| val.get("type")).and_then(Value::as_str) == Some("bodyweight_assisted_reps");
                        dict(Ok(format_exercise_history(id, val, summary, is_ass)))
                    }
                    Err(HevyError::Api { status: 404, .. }) => {
                        Outcome::Error(format!("exercise template {id} not found"))
                    }
                    Err(e) => dict(Err(e)),
                }
            }
            // ── Write ───────────────────────────────────────────────────────
            "create_routine" => {
                let routine = json!({
                    "title": a.get("title"),
                    "notes": a.get("notes"),
                    "folder_id": a.get("folder_id"),
                    "exercises": exercises(&a, false),
                });
                dict(match guard {
                    Ok(()) => hevy.create_routine(routine).await,
                    Err(e) => Err(e),
                })
            }
            "update_routine" => {
                let id = a.str("routine_id");
                dict(match guard {
                    Err(e) => Err(e),
                    Ok(()) => {
                        let notes = match a.0.get("notes") {
                            Some(v) => v.clone(),
                            None => match hevy.get_routine(id).await {
                                Ok(current) => {
                                    let cur_r = current.get("routine").unwrap_or(&current);
                                    cur_r.get("notes").cloned().unwrap_or(Value::Null)
                                }
                                Err(HevyError::Api { status: 404, .. }) => {
                                    return Outcome::Error(format!("routine {id} not found"));
                                }
                                Err(e) => return Outcome::Error(e.to_string()),
                            },
                        };
                        let routine = json!({
                            "title": a.get("title"),
                            "notes": notes,
                            "exercises": exercises(&a, false),
                        });
                        hevy.update_routine(id, routine).await
                    }
                })
            }
            "create_routine_folder" => dict(hevy.create_routine_folder(a.str("title")).await),
            "create_workout" => {
                let is_private = a.0.get("is_private").cloned().unwrap_or(json!(true));
                let workout = json!({
                    "title": a.get("title"),
                    "description": a.get("description"),
                    "start_time": a.get("start_time"),
                    "end_time": a.get("end_time"),
                    "is_private": is_private,
                    "exercises": exercises(&a, true),
                });
                dict(match guard {
                    Err(e) => Err(e),
                    Ok(()) => hevy.create_workout(workout).await,
                })
            }
            "update_workout" => {
                let id = a.str("workout_id");
                dict(match guard {
                    Err(e) => Err(e),
                    Ok(()) => {
                        let (is_private, description) = if a.0.contains_key("is_private")
                            && a.0.contains_key("description")
                        {
                            (a.get("is_private"), a.get("description"))
                        } else {
                            match hevy.get_workout(id).await {
                                Ok(current) => {
                                    let cur_w = current.get("workout").unwrap_or(&current);
                                    let p = match a.0.get("is_private") {
                                        Some(v) => v.clone(),
                                        None => cur_w.get("is_private").cloned().unwrap_or(json!(false)),
                                    };
                                    let d = match a.0.get("description") {
                                        Some(v) => v.clone(),
                                        None => cur_w.get("description").cloned().unwrap_or(Value::Null),
                                    };
                                    (p, d)
                                }
                                Err(HevyError::Api { status: 404, .. }) => {
                                    return Outcome::Error(format!("workout {id} not found"));
                                }
                                Err(e) => return Outcome::Error(e.to_string()),
                            }
                        };
                        let workout = json!({
                            "title": a.get("title"),
                            "description": description,
                            "start_time": a.get("start_time"),
                            "end_time": a.get("end_time"),
                            "is_private": is_private,
                            "exercises": exercises(&a, true),
                        });
                        hevy.update_workout(id, workout).await
                    }
                })
            }
            "create_exercise_template" => {
                let other = match a.get("other_muscles") {
                    Value::Array(items) if !items.is_empty() => Value::Array(items),
                    _ => json!([]),
                };
                let exercise = json!({
                    "title": a.get("title"),
                    "exercise_type": a.get("exercise_type"),
                    "equipment_category": a.get("equipment_category"),
                    "muscle_group": a.get("muscle_group"),
                    "other_muscles": other,
                });
                dict(hevy.create_exercise_template(exercise).await)
            }
            _ => Outcome::Dict(json!({"error": format!("Unexpected error: no handler for {name}")})),
        }
    }

    /// Find exercise templates by title substring, case-insensitive, scanning up
    /// to 10 pages × 100 templates (default library + custom ones). The library
    /// is English; a Cyrillic query is matched against the Russian titles the
    /// user's own workouts carry (see `ru_titles`).
    async fn search_exercise_templates(&self, query: &str, limit: i64) -> HevyResult<Vec<Value>> {
        let needle = fold(query.trim());
        let ru = if has_cyrillic(&needle) {
            self.ru_titles().await?
        } else {
            RU_TITLES.lock().await.as_ref().map(|(_, m)| m.clone()).unwrap_or_default()
        };
        let mut results: Vec<Value> = Vec::new();
        for page in 1..=10 {
            let envelope = self.hevy.get_exercise_templates(page, 100).await?;
            let batch = envelope
                .get("exercise_templates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for t in &batch {
                let id = t.get("id").and_then(Value::as_str).unwrap_or_default();
                let title = t.get("title").and_then(Value::as_str).unwrap_or_default();
                let title_ru = ru.get(id);
                let matches = fold(title).contains(&needle) || title_ru.is_some_and(|r| fold(r).contains(&needle));
                if matches {
                    let field = |k: &str| t.get(k).cloned().unwrap_or(Value::Null);
                    let ty_val = field("type");
                    if let (Some(id_str), Some(ty_str)) = (t.get("id").and_then(Value::as_str), ty_val.as_str()) {
                        EXERCISE_TYPES.lock().await.insert(id_str.to_string(), ty_str.to_string());
                    }
                    let mut entry = json!({
                        "id": field("id"),
                        "title": title,
                        "type": ty_val,
                        "primary_muscle_group": field("primary_muscle_group"),
                        "equipment": field("equipment"),
                        "is_custom": field("is_custom"),
                    });
                    if let Some(ru) = title_ru {
                        entry["title_ru"] = json!(ru);
                    }
                    results.push(entry);
                    if results.len() as i64 >= limit {
                        return Ok(results);
                    }
                }
            }
            let page_count = envelope.get("page_count").and_then(Value::as_i64).unwrap_or(page);
            if page >= page_count || batch.is_empty() {
                break;
            }
        }
        Ok(results)
    }

    /// `exercise_template_id → Russian title`, learnt from the most recent
    /// workouts: the Hevy app logs exercises under the user's language while
    /// the template library stays English. Newest title wins.
    async fn ru_titles(&self) -> HevyResult<HashMap<String, String>> {
        let mut cache = RU_TITLES.lock().await;
        if let Some((built, map)) = cache.as_ref()
            && built.elapsed() < RU_TITLES_TTL
        {
            return Ok(map.clone());
        }
        let mut map = HashMap::new();
        for page in 1..=RU_TITLES_PAGES {
            let envelope = match self.hevy.get_workouts(page, 10).await {
                Ok(envelope) => envelope,
                Err(HevyError::Api { status: 404, .. }) => break,
                Err(e) => return Err(e),
            };
            let workouts = envelope.get("workouts").and_then(Value::as_array).cloned().unwrap_or_default();
            for exercise in workouts.iter().filter_map(|w| w.get("exercises").and_then(Value::as_array)).flatten() {
                let id = exercise.get("exercise_template_id").and_then(Value::as_str);
                let title = exercise.get("title").and_then(Value::as_str);
                if let (Some(id), Some(title)) = (id, title)
                    && has_cyrillic(title)
                {
                    map.entry(id.to_string()).or_insert_with(|| title.to_string());
                }
            }
            let page_count = envelope.get("page_count").and_then(Value::as_i64).unwrap_or(page);
            if page >= page_count || workouts.is_empty() {
                break;
            }
        }
        *cache = Some((Instant::now(), map.clone()));
        Ok(map)
    }

    async fn is_assisted(&self, template_id: &str) -> bool {
        if template_id.is_empty() {
            return false;
        }
        {
            let cache = EXERCISE_TYPES.lock().await;
            if let Some(t) = cache.get(template_id) {
                return t == "bodyweight_assisted_reps";
            }
        }
        if let Ok(tmpl) = self.hevy.get_exercise_template(template_id).await {
            let ty = tmpl
                .get("type")
                .or_else(|| tmpl.get("exercise_type"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let assisted = ty == "bodyweight_assisted_reps";
            EXERCISE_TYPES.lock().await.insert(template_id.to_string(), ty);
            return assisted;
        }
        false
    }
}

static EXERCISE_TYPES: LazyLock<tokio::sync::Mutex<HashMap<String, String>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Process-wide: one server talks to one Hevy account.
static RU_TITLES: LazyLock<tokio::sync::Mutex<Option<(Instant, HashMap<String, String>)>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(None));
const RU_TITLES_TTL: Duration = Duration::from_secs(6 * 3600);
/// 10 pages × 10 workouts: the last ~100 sessions cover every exercise in use.
const RU_TITLES_PAGES: i64 = 10;

/// The Russian title already learnt for a template, without fetching anything.
async fn cached_title_ru(id: &str) -> Option<String> {
    RU_TITLES.lock().await.as_ref().and_then(|(_, m)| m.get(id).cloned())
}

fn has_cyrillic(s: &str) -> bool {
    s.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

/// Lowercase with ё folded into е, so "лежа" finds "Жим лёжа".
fn fold(s: &str) -> String {
    s.to_lowercase().replace('ё', "е")
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut cleaned = Map::new();
            for (k, v) in map {
                if !v.is_null() && k != "index" {
                    cleaned.insert(k, strip_nulls(v));
                }
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

fn normalize_iso_timestamp(ts: &str) -> String {
    let s = ts.trim();
    if s.is_empty() {
        return String::new();
    }
    let mut cleaned = if let Some(stripped) = s.strip_suffix("+00:00") {
        format!("{stripped}Z")
    } else {
        s.to_string()
    };
    if let Some(dot_idx) = cleaned.find('.') {
        if let Some(z_idx) = cleaned[dot_idx..].find('Z') {
            let before = &cleaned[..dot_idx];
            let after = &cleaned[dot_idx + z_idx..];
            cleaned = format!("{before}{after}");
        }
    }
    cleaned
}

fn to_local_moscow(utc_iso: &str) -> Option<String> {
    if utc_iso.len() < 19 || !utc_iso.ends_with('Z') {
        return None;
    }
    let date_part = &utc_iso[..10];
    let time_part = &utc_iso[11..19];
    let parts: Vec<&str> = date_part.split('-').collect();
    let tparts: Vec<&str> = time_part.split(':').collect();
    if parts.len() != 3 || tparts.len() != 3 {
        return None;
    }
    let mut y: i32 = parts[0].parse().ok()?;
    let mut m: i32 = parts[1].parse().ok()?;
    let mut d: i32 = parts[2].parse().ok()?;
    let mut hh: i32 = tparts[0].parse().ok()?;
    let mm: i32 = tparts[1].parse().ok()?;
    let ss: i32 = tparts[2].parse().ok()?;

    hh += 3;
    if hh >= 24 {
        hh -= 24;
        d += 1;
        let days_in_month = match m {
            2 => {
                if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                    29
                } else {
                    28
                }
            }
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        if d > days_in_month {
            d = 1;
            m += 1;
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
    }
    Some(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}+03:00"))
}

fn calc_duration_min(s: &str, e: &str) -> Option<i64> {
    if s.len() < 19 || e.len() < 19 {
        return None;
    }
    let t1_parts: Vec<&str> = s[11..19].split(':').collect();
    let t2_parts: Vec<&str> = e[11..19].split(':').collect();
    if t1_parts.len() != 3 || t2_parts.len() != 3 {
        return None;
    }
    let h1: i64 = t1_parts[0].parse().ok()?;
    let m1: i64 = t1_parts[1].parse().ok()?;
    let s1: i64 = t1_parts[2].parse().ok()?;
    let h2: i64 = t2_parts[0].parse().ok()?;
    let m2: i64 = t2_parts[1].parse().ok()?;
    let s2: i64 = t2_parts[2].parse().ok()?;

    let sec1 = h1 * 3600 + m1 * 60 + s1;
    let sec2 = h2 * 3600 + m2 * 60 + s2;
    if sec2 >= sec1 {
        Some((sec2 - sec1) / 60)
    } else {
        Some(((sec2 + 86400) - sec1) / 60)
    }
}

fn clean_workout(mut workout: Value) -> Value {
    if let Some(map) = workout.as_object_mut() {
        if let Some(w) = map.get_mut("workout") {
            if let Some(w_map) = w.as_object_mut() {
                if let Some(s) = w_map.get("start_time").and_then(Value::as_str) {
                    let norm = normalize_iso_timestamp(s);
                    if let Some(loc) = to_local_moscow(&norm) {
                        w_map.insert("start_time_local".to_string(), json!(loc));
                    }
                    w_map.insert("start_time".to_string(), json!(norm));
                }
                if let Some(e) = w_map.get("end_time").and_then(Value::as_str) {
                    w_map.insert("end_time".to_string(), json!(normalize_iso_timestamp(e)));
                }
            }
        } else {
            if let Some(s) = map.get("start_time").and_then(Value::as_str) {
                let norm = normalize_iso_timestamp(s);
                if let Some(loc) = to_local_moscow(&norm) {
                    map.insert("start_time_local".to_string(), json!(loc));
                }
                map.insert("start_time".to_string(), json!(norm));
            }
            if let Some(e) = map.get("end_time").and_then(Value::as_str) {
                map.insert("end_time".to_string(), json!(normalize_iso_timestamp(e)));
            }
        }
    }
    strip_nulls(workout)
}

impl HevyMcp {
    async fn format_workouts_list(&self, mut envelope: Value, detail: &str) -> Value {
        let Some(workouts) = envelope.get_mut("workouts").and_then(Value::as_array_mut) else {
            return strip_nulls(envelope);
        };
        if detail == "full" {
            for w in workouts.iter_mut() {
                if let Some(map) = w.as_object_mut() {
                    if let Some(s) = map.get("start_time").and_then(Value::as_str) {
                        let norm = normalize_iso_timestamp(s);
                        if let Some(loc) = to_local_moscow(&norm) {
                            map.insert("start_time_local".to_string(), json!(loc));
                        }
                        map.insert("start_time".to_string(), json!(norm));
                    }
                    if let Some(e) = map.get("end_time").and_then(Value::as_str) {
                        map.insert("end_time".to_string(), json!(normalize_iso_timestamp(e)));
                    }
                }
            }
            return strip_nulls(envelope);
        }
        let mut summary_workouts = Vec::with_capacity(workouts.len());
        for w in workouts.iter() {
            let empty_vec = Vec::new();
            let ex_list = w.get("exercises").and_then(Value::as_array).unwrap_or(&empty_vec);
            let mut ex_names = Vec::new();
            let mut total_vol = 0.0;
            for e in ex_list {
                if let Some(t) = e.get("title").and_then(Value::as_str) {
                    ex_names.push(json!(t));
                }
                let t_id = e.get("exercise_template_id").and_then(Value::as_str).unwrap_or_default();
                let is_assisted = e.get("type").or_else(|| e.get("exercise_type")).and_then(Value::as_str) == Some("bodyweight_assisted_reps")
                    || self.is_assisted(t_id).await;
                if !is_assisted {
                    if let Some(sets) = e.get("sets").and_then(Value::as_array) {
                        for s in sets {
                            let wt = s.get("weight_kg").and_then(Value::as_f64).unwrap_or(0.0);
                            let r = s.get("reps").and_then(Value::as_i64).unwrap_or(0);
                            total_vol += wt * (r as f64);
                        }
                    }
                }
            }
            let start_raw = w.get("start_time").and_then(Value::as_str).unwrap_or_default();
            let end_raw = w.get("end_time").and_then(Value::as_str).unwrap_or_default();
            let start_norm = normalize_iso_timestamp(start_raw);
            let end_norm = normalize_iso_timestamp(end_raw);
            let start_loc = to_local_moscow(&start_norm);
            let duration = calc_duration_min(&start_norm, &end_norm);

            let mut sum_item = json!({
                "id": w.get("id"),
                "title": w.get("title"),
                "start_time": start_norm,
                "end_time": end_norm,
                "exercises_count": ex_list.len(),
                "volume_kg": (total_vol * 100.0).round() / 100.0,
                "exercises": ex_names,
            });
            if let Some(loc) = start_loc {
                sum_item["start_time_local"] = json!(loc);
            }
            if let Some(d) = duration {
                sum_item["duration_min"] = json!(d);
            }
            summary_workouts.push(sum_item);
        }
        envelope["workouts"] = Value::Array(summary_workouts);
        strip_nulls(envelope)
    }
}

fn format_routines_list(mut envelope: Value, detail: &str) -> Value {
    let Some(routines) = envelope.get_mut("routines").and_then(Value::as_array_mut) else {
        return strip_nulls(envelope);
    };
    if detail == "full" {
        return strip_nulls(envelope);
    }
    let mut summary_routines = Vec::with_capacity(routines.len());
    for r in routines.iter() {
        let empty_vec = Vec::new();
        let ex_list = r.get("exercises").and_then(Value::as_array).unwrap_or(&empty_vec);
        let mut ex_names = Vec::new();
        for e in ex_list {
            if let Some(t) = e.get("title").and_then(Value::as_str) {
                ex_names.push(json!(t));
            }
        }
        let updated_at = r.get("updated_at").and_then(Value::as_str).map(normalize_iso_timestamp);
        summary_routines.push(json!({
            "id": r.get("id"),
            "title": r.get("title"),
            "folder_id": r.get("folder_id"),
            "updated_at": updated_at,
            "exercises": ex_names,
        }));
    }
    envelope["routines"] = Value::Array(summary_routines);
    strip_nulls(envelope)
}

fn format_exercise_history(template_id: &str, val: Value, summary: bool, is_assisted: bool) -> Value {
    let Some(history) = val.get("exercise_history").and_then(Value::as_array) else {
        return strip_nulls(val);
    };
    let mut workouts_order: Vec<String> = Vec::new();
    let mut workouts_map: Map<String, Value> = Map::new();

    for item in history {
        let Some(w_id) = item.get("workout_id").and_then(Value::as_str) else {
            continue;
        };
        if !workouts_map.contains_key(w_id) {
            workouts_order.push(w_id.to_string());
            let s_raw = item.get("workout_start_time").and_then(Value::as_str).unwrap_or_default();
            let e_raw = item.get("workout_end_time").and_then(Value::as_str).unwrap_or_default();
            let s_norm = normalize_iso_timestamp(s_raw);
            let e_norm = normalize_iso_timestamp(e_raw);
            let s_loc = to_local_moscow(&s_norm);
            let mut w_obj = json!({
                "workout_id": w_id,
                "title": item.get("workout_title").cloned().unwrap_or(Value::Null),
                "start_time": s_norm,
                "end_time": e_norm,
                "sets": [],
            });
            if let Some(loc) = s_loc {
                w_obj["start_time_local"] = json!(loc);
            }
            workouts_map.insert(w_id.to_string(), w_obj);
        }
        if let Some(w_entry) = workouts_map.get_mut(w_id) {
            if let Some(sets) = w_entry.get_mut("sets").and_then(Value::as_array_mut) {
                let mut set_obj = Map::new();
                for &k in &["weight_kg", "reps", "set_type", "rpe", "duration_seconds", "distance_meters", "custom_metric"] {
                    if let Some(v) = item.get(k) {
                        if !v.is_null() {
                            set_obj.insert(k.to_string(), v.clone());
                        }
                    }
                }
                set_obj.insert("set_index".to_string(), json!(sets.len() + 1));
                sets.push(Value::Object(set_obj));
            }
        }
    }

    let mut workouts_list = Vec::with_capacity(workouts_order.len());
    for w_id in workouts_order {
        if let Some(mut w) = workouts_map.remove(&w_id) {
            if summary {
                let empty_sets = Vec::new();
                let sets = w.get("sets").and_then(Value::as_array).unwrap_or(&empty_sets);
                let mut top_set: Option<Value> = None;
                let mut max_w = -1.0;
                let mut volume = 0.0;
                let mut best_e1rm = 0.0;
                let mut best_assisted: Option<(f64, i64)> = None;
                for s in sets {
                    let wt = s.get("weight_kg").and_then(Value::as_f64).unwrap_or(0.0);
                    let r = s.get("reps").and_then(Value::as_i64).unwrap_or(0);
                    if is_assisted {
                        let is_better = match best_assisted {
                            None => true,
                            Some((best_wt, best_r)) => wt < best_wt || (wt == best_wt && r > best_r),
                        };
                        if is_better {
                            best_assisted = Some((wt, r));
                            top_set = Some(json!({"weight_kg": wt, "reps": r}));
                        }
                    } else {
                        volume += wt * (r as f64);
                        let e1rm = if r <= 1 { wt } else { wt * (1.0 + (r as f64) / 30.0) };
                        if wt > max_w {
                            max_w = wt;
                            top_set = Some(json!({"weight_kg": wt, "reps": r}));
                        }
                        if e1rm > best_e1rm {
                            best_e1rm = e1rm;
                        }
                    }
                }
                w["top_set"] = top_set.unwrap_or(Value::Null);
                if !is_assisted {
                    w["volume_kg"] = json!((volume * 100.0).round() / 100.0);
                    w["estimated_1rm"] = json!((best_e1rm * 10.0).round() / 10.0);
                }
            }
            workouts_list.push(w);
        }
    }

    strip_nulls(json!({
        "common": {
            "exercise_template_id": template_id,
        },
        "workouts": workouts_list,
    }))
}
