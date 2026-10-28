//! The Hevy tool surface exposed to Claude, on the official `rmcp` SDK.
//!
//! Conventions (a stable contract the model can rely on):
//!   * Success — the tool's normal payload (a dict, or a list of dicts).
//!   * A recoverable problem (bad id, not configured, Hevy 4xx) — a dict
//!     `{"error": "<human message>"}`.
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

use std::sync::{Arc, LazyLock};

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
};

static TOOLS: LazyLock<Vec<Tool>> =
    LazyLock::new(|| serde_json::from_str(include_str!("tools.json")).expect("valid tools.json"));

fn server_info() -> Implementation {
    Implementation::new("Hevy", env!("CARGO_PKG_VERSION"))
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
}

fn err_dict(e: HevyError) -> Value {
    json!({"error": e.to_string()})
}

fn dict(result: HevyResult) -> Outcome {
    Outcome::Dict(result.unwrap_or_else(err_dict))
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
    CallToolResult::error(vec![ContentBlock::text(message)])
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
            "list_workouts" => dict(hevy.get_workouts(a.int("page"), a.int("page_size")).await),
            "get_workout" => dict(hevy.get_workout(a.str("workout_id")).await),
            "list_routines" => dict(hevy.get_routines(a.int("page"), a.int("page_size")).await),
            "get_routine" => dict(hevy.get_routine(a.str("routine_id")).await),
            "list_routine_folders" => {
                dict(hevy.get_routine_folders(a.int("page"), a.int("page_size")).await)
            }
            "get_routine_folder" => dict(hevy.get_routine_folder(a.int("folder_id")).await),
            "get_user_info" => dict(hevy.get_user_info().await),
            "search_exercise_templates" => {
                Outcome::List(self.search_exercise_templates(a.str("query"), a.int("limit")).await)
            }
            "get_exercise_template" => dict(hevy.get_exercise_template(a.str("template_id")).await),
            "get_exercise_history" => dict(
                hevy.get_exercise_history(
                    a.str("exercise_template_id"),
                    a.opt_str("start_date"),
                    a.opt_str("end_date"),
                )
                .await,
            ),
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
                let routine = json!({
                    "title": a.get("title"),
                    "notes": a.get("notes"),
                    "exercises": exercises(&a, false),
                });
                dict(match guard {
                    Ok(()) => hevy.update_routine(a.str("routine_id"), routine).await,
                    Err(e) => Err(e),
                })
            }
            "create_routine_folder" => dict(hevy.create_routine_folder(a.str("title")).await),
            "create_workout" | "update_workout" => {
                let workout = json!({
                    "title": a.get("title"),
                    "description": a.get("description"),
                    "start_time": a.get("start_time"),
                    "end_time": a.get("end_time"),
                    "is_private": a.get("is_private"),
                    "exercises": exercises(&a, true),
                });
                dict(match guard {
                    Err(e) => Err(e),
                    Ok(()) if name == "create_workout" => hevy.create_workout(workout).await,
                    Ok(()) => hevy.update_workout(a.str("workout_id"), workout).await,
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

    /// Find exercise templates by title substring (case-insensitive), scanning
    /// up to 10 pages × 100 templates (default library + custom ones).
    async fn search_exercise_templates(&self, query: &str, limit: i64) -> Vec<Value> {
        let needle = query.trim().to_lowercase();
        let mut results: Vec<Value> = Vec::new();
        for page in 1..=10 {
            let envelope = match self.hevy.get_exercise_templates(page, 100).await {
                Ok(envelope) => envelope,
                Err(e) => return vec![err_dict(e)],
            };
            let batch = envelope
                .get("exercise_templates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for t in &batch {
                let title = t.get("title").and_then(Value::as_str).unwrap_or_default();
                if title.to_lowercase().contains(&needle) {
                    let field = |k: &str| t.get(k).cloned().unwrap_or(Value::Null);
                    results.push(json!({
                        "id": field("id"),
                        "title": title,
                        "type": field("type"),
                        "primary_muscle_group": field("primary_muscle_group"),
                        "equipment": field("equipment"),
                        "is_custom": field("is_custom"),
                    }));
                    if results.len() as i64 >= limit {
                        return results;
                    }
                }
            }
            let page_count = envelope.get("page_count").and_then(Value::as_i64).unwrap_or(page);
            if page >= page_count || batch.is_empty() {
                break;
            }
        }
        if results.is_empty() {
            return vec![json!({"error": format!("No exercise templates matched '{query}'.")})];
        }
        results
    }
}
