"""Async Hevy REST client over the ``/v1`` API.

Authenticated with the personal ``api-key`` header. Constructed lazily so the app
boots before Hevy is configured; only an actual request needs the key.

Docs: https://api.hevyapp.com/docs/
  * List endpoints are paginated: ``{"page", "page_count", <collection>: [...]}``.
  * ``pageSize`` caps at 10 for workouts/routines, 100 for exercise_templates.
  * The public API exposes no DELETE endpoints.
"""
from __future__ import annotations

import logging
from typing import Any, Optional

import httpx

from hevy_mcp.config import Config, load_config

logger = logging.getLogger(__name__)

_DEFAULT_TIMEOUT = 30.0


class HevyNotConfigured(RuntimeError):
    """Raised when a request is attempted without ``HEVY_API_KEY``."""


class HevyAPIError(RuntimeError):
    """Non-2xx response from the Hevy API (status code carried for the caller)."""

    def __init__(self, status_code: int, message: str):
        self.status_code = status_code
        self.message = message
        super().__init__(f"Hevy API {status_code}: {message}")


class HevyClient:
    def __init__(self, config: Optional[Config] = None):
        self._config = config or load_config()

    @classmethod
    def from_config(cls, config: Optional[Config] = None) -> "HevyClient":
        return cls(config)

    @property
    def is_configured(self) -> bool:
        return bool(self._config.hevy_api_key)

    def _headers(self) -> dict[str, str]:
        if not self._config.hevy_api_key:
            raise HevyNotConfigured("HEVY_API_KEY is not set")
        return {
            "api-key": self._config.hevy_api_key,
            "Accept": "application/json",
            "Content-Type": "application/json",
        }

    async def _request(
        self,
        method: str,
        path: str,
        *,
        params: Optional[dict] = None,
        json: Optional[dict] = None,
    ) -> Any:
        url = f"{self._config.hevy_base_url.rstrip('/')}{path}"
        async with httpx.AsyncClient(timeout=_DEFAULT_TIMEOUT) as client:
            resp = await client.request(
                method, url, headers=self._headers(), params=params, json=json
            )
        if resp.status_code >= 400:
            raise HevyAPIError(resp.status_code, resp.text[:400])
        # 201/204 with an empty body → return {} rather than choke on json().
        if not resp.content:
            return {}
        return resp.json()

    # ── Workouts ──────────────────────────────────────────────────────────────
    async def workout_count(self) -> int:
        data = await self._request("GET", "/v1/workouts/count")
        return int(data.get("workout_count", 0))

    async def get_workouts(self, page: int = 1, page_size: int = 10) -> dict:
        page_size = min(max(1, page_size), 10)
        return await self._request(
            "GET", "/v1/workouts", params={"page": page, "pageSize": page_size}
        )

    async def get_workout(self, workout_id: str) -> dict:
        return await self._request("GET", f"/v1/workouts/{workout_id}")

    async def create_workout(self, workout: dict) -> dict:
        return await self._request("POST", "/v1/workouts", json={"workout": workout})

    async def update_workout(self, workout_id: str, workout: dict) -> dict:
        return await self._request(
            "PUT", f"/v1/workouts/{workout_id}", json={"workout": workout}
        )

    # ── Routines ──────────────────────────────────────────────────────────────
    async def get_routines(self, page: int = 1, page_size: int = 10) -> dict:
        page_size = min(max(1, page_size), 10)
        return await self._request(
            "GET", "/v1/routines", params={"page": page, "pageSize": page_size}
        )

    async def get_routine(self, routine_id: str) -> dict:
        return await self._request("GET", f"/v1/routines/{routine_id}")

    async def create_routine(self, routine: dict) -> dict:
        return await self._request("POST", "/v1/routines", json={"routine": routine})

    async def update_routine(self, routine_id: str, routine: dict) -> dict:
        return await self._request(
            "PUT", f"/v1/routines/{routine_id}", json={"routine": routine}
        )

    # ── Routine folders ───────────────────────────────────────────────────────
    async def get_routine_folders(self, page: int = 1, page_size: int = 10) -> dict:
        page_size = min(max(1, page_size), 10)
        return await self._request(
            "GET", "/v1/routine_folders", params={"page": page, "pageSize": page_size}
        )

    async def get_routine_folder(self, folder_id: int | str) -> dict:
        return await self._request("GET", f"/v1/routine_folders/{folder_id}")

    async def create_routine_folder(self, title: str) -> dict:
        return await self._request(
            "POST", "/v1/routine_folders", json={"routine_folder": {"title": title}}
        )

    # ── Exercise templates ────────────────────────────────────────────────────
    async def get_exercise_templates(self, page: int = 1, page_size: int = 100) -> dict:
        page_size = min(max(1, page_size), 100)
        return await self._request(
            "GET",
            "/v1/exercise_templates",
            params={"page": page, "pageSize": page_size},
        )

    async def get_exercise_template(self, template_id: str) -> dict:
        return await self._request("GET", f"/v1/exercise_templates/{template_id}")

    async def create_exercise_template(self, exercise: dict) -> dict:
        return await self._request(
            "POST", "/v1/exercise_templates", json={"exercise": exercise}
        )

    # ── Exercise history ──────────────────────────────────────────────────────
    async def get_exercise_history(
        self,
        template_id: str,
        start_date: Optional[str] = None,
        end_date: Optional[str] = None,
    ) -> dict:
        """Every set ever performed for one exercise, flat. Unpaginated — the
        date window is the only way to bound the response size."""
        params = {
            k: v
            for k, v in (("start_date", start_date), ("end_date", end_date))
            if v
        }
        return await self._request(
            "GET", f"/v1/exercise_history/{template_id}", params=params or None
        )

    # ── Account ───────────────────────────────────────────────────────────────
    async def get_user_info(self) -> dict:
        return await self._request("GET", "/v1/user/info")
