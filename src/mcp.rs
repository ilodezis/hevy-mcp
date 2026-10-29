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
            "get_exercise_template" => {
                let id = a.str("template_id");
                let mut res = hevy.get_exercise_template(id).await;
                if let Ok(Value::Object(ref mut map)) = res {
                    map.remove("thumbnail_url");
                    if let Some(ru) = find_title_ru(id) {
                        map.insert("title_ru".to_string(), json!(ru));
                    }
                }
                dict(res)
            }
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
                                Err(e) => return Outcome::Dict(err_dict(e)),
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
                                Err(e) => return Outcome::Dict(err_dict(e)),
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

    /// Find exercise templates by title substring (case-insensitive in English or Russian),
    /// scanning up to 10 pages × 100 templates (default library + custom ones).
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
                let id = t.get("id").and_then(Value::as_str).unwrap_or_default();
                let title = t.get("title").and_then(Value::as_str).unwrap_or_default();
                let title_ru = find_title_ru(id);
                let ru_keywords = ru_keywords_for_en(title);

                let matches_en = title.to_lowercase().contains(&needle);
                let matches_ru_alias = title_ru.map(|r| r.to_lowercase().contains(&needle)).unwrap_or(false);
                let matches_ru_kw = ru_keywords.iter().any(|&kw| needle.contains(kw) || kw.contains(&needle));

                if matches_en || matches_ru_alias || matches_ru_kw {
                    let field = |k: &str| t.get(k).cloned().unwrap_or(Value::Null);
                    let mut entry = json!({
                        "id": field("id"),
                        "title": title,
                        "type": field("type"),
                        "primary_muscle_group": field("primary_muscle_group"),
                        "equipment": field("equipment"),
                        "is_custom": field("is_custom"),
                    });
                    if let Some(ru) = title_ru {
                        entry["title_ru"] = json!(ru);
                    }
                    results.push(entry);
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
        results
    }
}

struct ExerciseAlias {
    id: &'static str,
    title_ru: &'static str,
}

static KNOWN_EXERCISES: &[ExerciseAlias] = &[
    ExerciseAlias { id: "7EB3F7C3", title_ru: "Жим от груди (Тренажёр)" },
    ExerciseAlias { id: "0EB695C9", title_ru: "Горизонтальный жим ногами (Тренажёр)" },
    ExerciseAlias { id: "0EB695CA", title_ru: "Жим ногами (Тренажёр)" },
    ExerciseAlias { id: "D05F25F4", title_ru: "Жим лёжа (Штанга)" },
    ExerciseAlias { id: "4B1E94D8", title_ru: "Жим лёжа на наклонной скамье (Штанга)" },
    ExerciseAlias { id: "B42CEBF6", title_ru: "Жим лёжа (Гантели)" },
    ExerciseAlias { id: "3A29D843", title_ru: "Жим лёжа на наклонной скамье (Гантели)" },
    ExerciseAlias { id: "5A8696F6", title_ru: "Армейский жим (Штанга)" },
    ExerciseAlias { id: "E6BFB194", title_ru: "Жим сидя (Гантели)" },
    ExerciseAlias { id: "397DE247", title_ru: "Приседания (Штанга)" },
    ExerciseAlias { id: "4920CEB3", title_ru: "Становая тяга (Штанга)" },
    ExerciseAlias { id: "5133BF9F", title_ru: "Румынская тяга (Штанга)" },
    ExerciseAlias { id: "D9E6C3B7", title_ru: "Тяга верхнего блока" },
    ExerciseAlias { id: "6DBC5A2F", title_ru: "Тяга горизонтального блока" },
    ExerciseAlias { id: "98E56B12", title_ru: "Тяга штанги в наклоне" },
    ExerciseAlias { id: "95DE73B1", title_ru: "Сгибание рук на бицепс (Гантели)" },
    ExerciseAlias { id: "F8B12C45", title_ru: "Разгибание рук на трицепс" },
    ExerciseAlias { id: "A1B2C3D4", title_ru: "Махи гантелями в стороны" },
    ExerciseAlias { id: "B2C3D4E5", title_ru: "Разгибание ног в тренажёре" },
    ExerciseAlias { id: "C3D4E5F6", title_ru: "Сгибание ног в тренажёре" },
    ExerciseAlias { id: "D4E5F607", title_ru: "Подтягивания" },
    ExerciseAlias { id: "E5F60718", title_ru: "Отжимания на брусьях" },
];

fn find_title_ru(id: &str) -> Option<&'static str> {
    KNOWN_EXERCISES.iter().find(|e| e.id.eq_ignore_ascii_case(id)).map(|e| e.title_ru)
}

fn ru_keywords_for_en(en: &str) -> &'static [&'static str] {
    let lower = en.to_lowercase();
    if lower.contains("press") {
        &["жим"]
    } else if lower.contains("squat") {
        &["присед", "приседания"]
    } else if lower.contains("deadlift") || lower.contains("row") || lower.contains("pulldown") {
        &["тяга"]
    } else if lower.contains("curl") {
        &["бицепс", "сгибание"]
    } else if lower.contains("tricep") || lower.contains("extension") {
        &["трицепс", "разгибание"]
    } else if lower.contains("pull up") {
        &["подтягивания"]
    } else if lower.contains("dip") {
        &["отжимания", "брусья"]
    } else {
        &[]
    }
}
