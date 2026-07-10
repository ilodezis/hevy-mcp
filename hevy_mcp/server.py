"""FastMCP server — the Hevy tool surface exposed to Claude.

Conventions (a stable contract the model can rely on):
  * Success — the tool's normal payload (a dict, or a list of dicts).
  * A recoverable problem (bad id, not configured, Hevy 4xx) — a dict
    ``{"error": "<human message>"}``.

Building a routine is a two-step dance the model should follow:
  1. ``search_exercise_templates("bench press")`` → get the ``exercise_template_id``.
  2. ``create_routine(...)`` with those ids and the set scheme.
Exercise template ids are stable, so once discovered they can be reused.
"""
from __future__ import annotations

import logging
from typing import Literal, Optional

from fastmcp import FastMCP
from pydantic import BaseModel, Field

from hevy_mcp.client import HevyAPIError, HevyClient, HevyNotConfigured

logger = logging.getLogger(__name__)

# Stateless mode for cloud/OAuth deployment compatibility.
mcp = FastMCP("Hevy")

SetType = Literal["warmup", "normal", "failure", "dropset"]


def _client() -> HevyClient:
    return HevyClient.from_config()


def _err(exc: Exception) -> dict:
    if isinstance(exc, HevyNotConfigured):
        return {"error": "Hevy is not configured — HEVY_API_KEY is missing on the server."}
    if isinstance(exc, HevyAPIError):
        return {"error": f"Hevy API returned {exc.status_code}: {exc.message}"}
    logger.exception("Unexpected error in Hevy tool")
    return {"error": f"Unexpected error: {exc}"}


# ── Input models (give Claude a precise JSON schema) ──────────────────────────
class SetInput(BaseModel):
    """One set. Provide the fields relevant to the exercise type: weight_kg+reps
    for lifts, duration_seconds for planks/cardio, distance_meters for runs."""
    type: SetType = Field("normal", description="warmup | normal | failure | dropset")
    weight_kg: Optional[float] = Field(None, description="Weight in kilograms")
    reps: Optional[int] = None
    distance_meters: Optional[float] = None
    duration_seconds: Optional[int] = None
    rpe: Optional[float] = Field(None, description="Rate of perceived exertion 6–10 (workouts only)")
    custom_metric: Optional[float] = None


class ExerciseInput(BaseModel):
    """One exercise slot. ``exercise_template_id`` comes from
    ``search_exercise_templates``. Group exercises into a superset by giving them
    the same integer ``superset_id`` (null = not in a superset)."""
    exercise_template_id: str
    sets: list[SetInput]
    superset_id: Optional[int] = None
    rest_seconds: Optional[int] = Field(None, description="Rest after this exercise (routines)")
    notes: Optional[str] = None


def _routine_set_dict(s: SetInput) -> dict:
    return {
        "type": s.type,
        "weight_kg": s.weight_kg,
        "reps": s.reps,
        "distance_meters": s.distance_meters,
        "duration_seconds": s.duration_seconds,
        "custom_metric": s.custom_metric,
    }


def _workout_set_dict(s: SetInput) -> dict:
    d = _routine_set_dict(s)
    d["rpe"] = s.rpe
    return d


def _routine_exercises(exercises: list[ExerciseInput]) -> list[dict]:
    return [
        {
            "exercise_template_id": e.exercise_template_id,
            "superset_id": e.superset_id,
            "rest_seconds": e.rest_seconds,
            "notes": e.notes,
            "sets": [_routine_set_dict(s) for s in e.sets],
        }
        for e in exercises
    ]


def _workout_exercises(exercises: list[ExerciseInput]) -> list[dict]:
    return [
        {
            "exercise_template_id": e.exercise_template_id,
            "superset_id": e.superset_id,
            "notes": e.notes,
            "sets": [_workout_set_dict(s) for s in e.sets],
        }
        for e in exercises
    ]


# ══════════════════════════════════════════════════════════════════════════════
# READ
# ══════════════════════════════════════════════════════════════════════════════
@mcp.tool()
async def workout_count() -> dict:
    """Total number of logged workouts on the Hevy account."""
    try:
        return {"workout_count": await _client().workout_count()}
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def list_workouts(page: int = 1, page_size: int = 10) -> dict:
    """List logged workouts, newest first (paginated; page_size max 10).
    Returns the Hevy envelope: {page, page_count, workouts: [...]}."""
    try:
        return await _client().get_workouts(page=page, page_size=page_size)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def get_workout(workout_id: str) -> dict:
    """Fetch a single workout by id (full exercise → set tree)."""
    try:
        return await _client().get_workout(workout_id)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def list_routines(page: int = 1, page_size: int = 10) -> dict:
    """List saved routines / training plans (paginated; page_size max 10)."""
    try:
        return await _client().get_routines(page=page, page_size=page_size)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def get_routine(routine_id: str) -> dict:
    """Fetch a single routine by id (its full exercise → set structure)."""
    try:
        return await _client().get_routine(routine_id)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def list_routine_folders(page: int = 1, page_size: int = 10) -> dict:
    """List routine folders (e.g. Push / Pull / Legs splits)."""
    try:
        return await _client().get_routine_folders(page=page, page_size=page_size)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def search_exercise_templates(query: str, limit: int = 20) -> list[dict]:
    """Find exercise templates by title substring (case-insensitive).

    Returns a list of {id, title, type, primary_muscle_group, equipment, is_custom}.
    Use the ``id`` as ``exercise_template_id`` when building routines/workouts.
    Scans up to ~1000 templates (default library + your custom ones).
    """
    try:
        needle = query.strip().lower()
        results: list[dict] = []
        client = _client()
        for page in range(1, 11):  # up to 10 pages × 100 = 1000 templates
            envelope = await client.get_exercise_templates(page=page, page_size=100)
            batch = envelope.get("exercise_templates") or []
            for t in batch:
                title = (t.get("title") or "")
                if needle in title.lower():
                    results.append({
                        "id": t.get("id"),
                        "title": title,
                        "type": t.get("type"),
                        "primary_muscle_group": t.get("primary_muscle_group"),
                        "equipment": t.get("equipment"),
                        "is_custom": t.get("is_custom"),
                    })
                    if len(results) >= limit:
                        return results
            if page >= int(envelope.get("page_count", page)) or not batch:
                break
        return results or [{"error": f"No exercise templates matched '{query}'."}]
    except Exception as exc:  # noqa: BLE001
        return [_err(exc)]


@mcp.tool()
async def get_exercise_template(template_id: str) -> dict:
    """Fetch a single exercise template by id."""
    try:
        return await _client().get_exercise_template(template_id)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


# ══════════════════════════════════════════════════════════════════════════════
# WRITE
# ══════════════════════════════════════════════════════════════════════════════
@mcp.tool()
async def create_routine(
    title: str,
    exercises: list[ExerciseInput],
    notes: Optional[str] = None,
    folder_id: Optional[int] = None,
) -> dict:
    """Create a new routine (a reusable training plan / workout template).

    ``exercises`` is an ordered list; each needs an ``exercise_template_id`` (from
    ``search_exercise_templates``) and its ``sets``. For routines, set targets are
    the planned weight/reps. Returns the created routine (with its new id).
    """
    try:
        routine = {
            "title": title,
            "notes": notes,
            "folder_id": folder_id,
            "exercises": _routine_exercises(exercises),
        }
        return await _client().create_routine(routine)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def update_routine(
    routine_id: str,
    title: str,
    exercises: list[ExerciseInput],
    notes: Optional[str] = None,
) -> dict:
    """Replace an existing routine's contents (title, notes, full exercise list).

    This overwrites the routine — fetch it with ``get_routine`` first if you only
    want to tweak part of it, then send the complete desired state.
    """
    try:
        routine = {
            "title": title,
            "notes": notes,
            "exercises": _routine_exercises(exercises),
        }
        return await _client().update_routine(routine_id, routine)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def create_routine_folder(title: str) -> dict:
    """Create a routine folder (e.g. 'Push/Pull/Legs'). Returns it with its id,
    which you can pass as ``folder_id`` to ``create_routine``."""
    try:
        return await _client().create_routine_folder(title)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def create_workout(
    title: str,
    start_time: str,
    end_time: str,
    exercises: list[ExerciseInput],
    description: Optional[str] = None,
    is_private: bool = False,
) -> dict:
    """Log a *completed* workout session.

    ``start_time`` / ``end_time`` are ISO-8601 timestamps (e.g.
    '2026-07-10T18:30:00Z'). Set weights/reps are what was actually performed;
    ``rpe`` is allowed here. Returns the created workout with its id.
    """
    try:
        workout = {
            "title": title,
            "description": description,
            "start_time": start_time,
            "end_time": end_time,
            "is_private": is_private,
            "exercises": _workout_exercises(exercises),
        }
        return await _client().create_workout(workout)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def update_workout(
    workout_id: str,
    title: str,
    start_time: str,
    end_time: str,
    exercises: list[ExerciseInput],
    description: Optional[str] = None,
    is_private: bool = False,
) -> dict:
    """Overwrite an existing logged workout with the full desired state.
    Fetch it with ``get_workout`` first if you only mean to edit part of it."""
    try:
        workout = {
            "title": title,
            "description": description,
            "start_time": start_time,
            "end_time": end_time,
            "is_private": is_private,
            "exercises": _workout_exercises(exercises),
        }
        return await _client().update_workout(workout_id, workout)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)


@mcp.tool()
async def create_exercise_template(
    title: str,
    exercise_type: Literal[
        "weight_reps", "reps_only", "bodyweight_reps", "bodyweight_assisted_reps",
        "duration", "weight_duration", "distance_duration", "short_distance_weight",
    ],
    equipment_category: Literal[
        "none", "barbell", "dumbbell", "kettlebell", "machine", "plate",
        "resistance_band", "suspension", "other",
    ],
    muscle_group: str,
    other_muscles: Optional[list[str]] = None,
) -> dict:
    """Create a custom exercise template (only if none of the existing ones fit —
    search first). ``muscle_group`` is a Hevy enum e.g. 'chest', 'quadriceps',
    'lats'. Note: some Hevy plans may not permit custom exercises via the API."""
    try:
        exercise = {
            "title": title,
            "exercise_type": exercise_type,
            "equipment_category": equipment_category,
            "muscle_group": muscle_group,
            "other_muscles": other_muscles or [],
        }
        return await _client().create_exercise_template(exercise)
    except Exception as exc:  # noqa: BLE001
        return _err(exc)
