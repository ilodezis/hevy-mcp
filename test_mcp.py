"""Offline integration coverage: python -m unittest -v test_mcp.

Exercise the real HTTP mount, OAuth flow and all tools. Only outbound Hevy HTTP
is replaced; no real account or credentials are used.
"""
from __future__ import annotations

import base64
from functools import wraps
import hashlib
import json
import os
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

import httpx

TEST_ENV = {
    "HEVY_MCP_SESSION_SECRET": "integration-test-signing-secret",
    "HEVY_MCP_PASSWORD": "integration-password",
    "HEVY_MCP_CLIENT_ID": "integration-client",
    "HEVY_MCP_CLIENT_SECRET": "integration-client-secret",
    "HEVY_MCP_REDIRECT_URIS": "https://client.example/callback",
    "HEVY_MCP_COOKIE_SECURE": "false",
    "HEVY_API_KEY": "integration-api-key",
    "HEVY_BASE_URL": "https://hevy.example",
    "FASTMCP_MCP_CAMELCASE_COMPAT": "false",
}

with patch.dict(os.environ, TEST_ENV):
    from hevy_mcp.app import create_app
    from hevy_mcp.auth import create_session, get_mcp_serializer
    from hevy_mcp.config import load_config

ACCEPT = "application/json, text/event-stream"
EXERCISES = [{
    "exercise_template_id": "bench", "superset_id": 1, "rest_seconds": 90,
    "notes": "Controlled", "sets": [{"weight_kg": 60, "reps": 8, "rpe": 8}],
}]
WORKOUT = {
    "title": "Test workout", "start_time": "2026-09-01T10:00:00Z",
    "end_time": "2026-09-01T11:00:00Z", "exercises": EXERCISES,
}
CASES = [
    ("workout_count", {}, "GET", "/v1/workouts/count"),
    ("list_workouts", {"page_size": 50}, "GET", "/v1/workouts"),
    ("get_workout", {"workout_id": "w1"}, "GET", "/v1/workouts/w1"),
    ("list_routines", {}, "GET", "/v1/routines"),
    ("get_routine", {"routine_id": "r1"}, "GET", "/v1/routines/r1"),
    ("list_routine_folders", {}, "GET", "/v1/routine_folders"),
    ("get_routine_folder", {"folder_id": 42}, "GET", "/v1/routine_folders/42"),
    ("get_user_info", {}, "GET", "/v1/user/info"),
    ("search_exercise_templates", {"query": "bench"}, "GET", "/v1/exercise_templates"),
    ("get_exercise_template", {"template_id": "bench"}, "GET", "/v1/exercise_templates/bench"),
    ("get_exercise_history", {"exercise_template_id": "bench", "start_date": "2026-01-01T00:00:00Z"}, "GET", "/v1/exercise_history/bench"),
    ("create_routine", {"title": "Test routine", "exercises": EXERCISES}, "POST", "/v1/routines"),
    ("update_routine", {"routine_id": "r1", "title": "Test routine", "exercises": EXERCISES}, "PUT", "/v1/routines/r1"),
    ("create_routine_folder", {"title": "Test folder"}, "POST", "/v1/routine_folders"),
    ("create_workout", WORKOUT, "POST", "/v1/workouts"),
    ("update_workout", {"workout_id": "w1", **WORKOUT}, "PUT", "/v1/workouts/w1"),
    ("create_exercise_template", {"title": "Test exercise", "exercise_type": "weight_reps", "equipment_category": "barbell", "muscle_group": "chest"}, "POST", "/v1/exercise_templates"),
]


def payload(response):
    for line in response.text.splitlines():
        if line.startswith("data:"):
            return json.loads(line[5:])
    return response.json()


def with_app(test):
    """Enter/exit AnyIO's lifespan in the same task as the test."""
    @wraps(test)
    async def run(self):
        async with self.app.router.lifespan_context(self.app):
            async with httpx.AsyncClient(
                transport=httpx.ASGITransport(app=self.app), base_url="http://check",
            ) as self.http:
                await test(self)
    return run


class MCPIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.env = patch.dict(os.environ, TEST_ENV)
        self.env.start()
        self.addCleanup(self.env.stop)
        load_config.cache_clear()
        self.addCleanup(load_config.cache_clear)
        self.app = create_app()
        self.token = get_mcp_serializer().dumps({
            "type": "mcp_access_token", "client_id": TEST_ENV["HEVY_MCP_CLIENT_ID"],
        })
        self.auth = {"Authorization": f"Bearer {self.token}", "Accept": ACCEPT}

    async def rpc(self, method, params=None, headers=None):
        params = dict(params or {})
        headers = dict(headers or self.auth)
        if self.auth.get("MCP-Protocol-Version") == "2026-07-28":
            headers["mcp-method"] = method
            if method == "tools/call":
                headers["mcp-name"] = params["name"]
            params["_meta"] = {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            }
        response = await self.http.post("/mcp/", headers=headers, json={
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        })
        self.assertEqual(response.status_code, 200, response.text)
        result = payload(response)
        self.assertNotIn("error", result, result)
        return result["result"]

    async def connect(self, modern=False):
        self.auth["MCP-Protocol-Version"] = "2026-07-28" if modern else "2025-06-18"
        if modern:
            return await self.rpc("server/discover")
        response = await self.http.post("/mcp/", headers=self.auth, json={
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "integration-test", "version": "1"},
            },
        })
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(payload(response)["result"]["serverInfo"]["name"], "Hevy")
        self.auth["mcp-session-id"] = response.headers["mcp-session-id"]
        response = await self.http.post("/mcp/", headers=self.auth, json={
            "jsonrpc": "2.0", "method": "notifications/initialized",
        })
        self.assertEqual(response.status_code, 202)

    @with_app
    async def test_health_and_auth_for_both_protocols(self):
        self.assertEqual((await self.http.get("/")).json()["status"], "ok")
        wrong_client = get_mcp_serializer().dumps({"type": "mcp_access_token", "client_id": "other"})
        for version in ("2025-06-18", "2026-07-28"):
            for token in (None, "forged", create_session(), wrong_client):
                with self.subTest(version=version, token_kind=bool(token)):
                    headers = {"Accept": ACCEPT, "MCP-Protocol-Version": version}
                    if token:
                        headers["Authorization"] = f"Bearer {token}"
                    response = await self.http.post("/mcp/", headers=headers, json={
                        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
                    })
                    self.assertEqual(response.status_code, 401)
                    self.assertEqual(response.headers["www-authenticate"], "Bearer")
        response = await self.http.post(f"/mcp/?token={self.token}", json={})
        self.assertEqual(response.status_code, 401)

    async def exercise_tools(self, modern):
        await self.connect(modern)
        tools = (await self.rpc("tools/list"))["tools"]
        self.assertEqual({t["name"] for t in tools}, {c[0] for c in CASES})
        schema = next(t["inputSchema"] for t in tools if t["name"] == "create_workout")
        self.assertIn("exercises", schema["required"])
        self.assertIn("sets", schema["properties"]["exercises"]["items"]["required"])
        original_client = httpx.AsyncClient
        for name, args, method, path in CASES:
            with self.subTest(tool=name, modern=modern):
                requests = []
                upstream = {"ok": name}
                if name == "workout_count":
                    upstream = {"workout_count": 12}
                elif name == "search_exercise_templates":
                    upstream = {"page_count": 1, "exercise_templates": [{
                        "id": "bench", "title": "Bench press", "type": "weight_reps",
                        "primary_muscle_group": "chest", "equipment": "barbell", "is_custom": False,
                    }]}

                def respond(request):
                    requests.append(request)
                    return httpx.Response(200, json=upstream)

                def client_factory(**kwargs):
                    return original_client(transport=httpx.MockTransport(respond), **kwargs)

                with patch("hevy_mcp.client.httpx.AsyncClient", side_effect=client_factory):
                    result = await self.rpc("tools/call", {"name": name, "arguments": args})
                self.assertFalse(result.get("isError", False), result)
                decoded = json.loads(result["content"][0]["text"])
                expected = upstream["exercise_templates"] if name == "search_exercise_templates" else upstream
                self.assertEqual(decoded, expected)
                self.assertEqual(len(requests), 1)
                request = requests[0]
                self.assertEqual((request.method, request.url.path), (method, path))
                self.assertEqual(request.headers["api-key"], "integration-api-key")
                if name == "list_workouts":
                    self.assertEqual(request.url.params["pageSize"], "10")
                if name == "get_exercise_history":
                    self.assertEqual(dict(request.url.params), {"start_date": args["start_date"]})
                if "exercises" in args:
                    kind = "routine" if "routine" in name else "workout"
                    sent = json.loads(request.content)[kind]
                    exercise = sent["exercises"][0]
                    self.assertEqual(exercise["exercise_template_id"], "bench")
                    self.assertEqual(exercise["superset_id"], 1)
                    self.assertEqual(exercise["sets"][0]["weight_kg"], 60)
                    self.assertEqual(exercise["sets"][0]["type"], "normal")
                    if kind == "routine":
                        self.assertEqual(exercise["rest_seconds"], 90)
                        self.assertNotIn("rpe", exercise["sets"][0])
                    else:
                        self.assertEqual(exercise["sets"][0]["rpe"], 8)
                        self.assertNotIn("rest_seconds", exercise)

    @with_app
    async def test_all_tools_legacy(self):
        await self.exercise_tools(modern=False)

    @with_app
    async def test_all_tools_modern(self):
        await self.exercise_tools(modern=True)

    @with_app
    async def test_validation_and_upstream_errors(self):
        await self.connect()
        original_client = httpx.AsyncClient
        def client_factory(**kwargs):
            return original_client(transport=httpx.MockTransport(
                lambda request: httpx.Response(429, text="Try later")), **kwargs)
        with patch("hevy_mcp.client.httpx.AsyncClient", side_effect=client_factory) as factory:
            for args in ({}, {**WORKOUT, "exercises": [{"sets": []}]}):
                result = await self.rpc("tools/call", {"name": "create_workout", "arguments": args})
                self.assertTrue(result["isError"])
            factory.assert_not_called()
            result = await self.rpc("tools/call", {"name": "workout_count", "arguments": {}})
        self.assertEqual(json.loads(result["content"][0]["text"]), {"error": "Hevy API returned 429: Try later"})
        with patch.dict(os.environ, {"HEVY_API_KEY": ""}):
            load_config.cache_clear()
            result = await self.rpc("tools/call", {"name": "workout_count", "arguments": {}})
        self.assertIn("HEVY_API_KEY is missing", json.loads(result["content"][0]["text"])["error"])

    @with_app
    async def test_oauth_login_pkce_and_single_use_code(self):
        metadata = (await self.http.get("/.well-known/oauth-authorization-server")).json()
        self.assertEqual(metadata["token_endpoint"], "http://check/oauth/token")
        verifier = "a" * 43
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        params = {
            "client_id": TEST_ENV["HEVY_MCP_CLIENT_ID"], "redirect_uri": "https://client.example/callback",
            "state": "test-state", "code_challenge": challenge, "code_challenge_method": "S256",
        }
        response = await self.http.get("/oauth/authorize", params={"response_type": "code", **params})
        self.assertEqual(response.status_code, 302)
        self.assertTrue(response.headers["location"].startswith("/login?"))
        self.assertEqual((await self.http.get("/login")).status_code, 200)
        self.assertEqual((await self.http.post("/login", data={"password": "wrong"})).status_code, 401)
        self.assertEqual((await self.http.post("/login", data={"password": TEST_ENV["HEVY_MCP_PASSWORD"]})).status_code, 303)
        self.assertEqual((await self.http.get("/oauth/authorize", params={"response_type": "code", **params})).status_code, 200)
        response = await self.http.post("/oauth/authorize/approve", data=params)
        self.assertEqual(response.status_code, 302)
        query = parse_qs(urlsplit(response.headers["location"]).query)
        self.assertEqual(query["state"], ["test-state"])
        exchange = {
            "grant_type": "authorization_code", "code": query["code"][0],
            "client_id": params["client_id"], "client_secret": TEST_ENV["HEVY_MCP_CLIENT_SECRET"],
            "redirect_uri": params["redirect_uri"], "code_verifier": verifier,
        }
        response = await self.http.post("/oauth/token", data=exchange)
        self.assertEqual(response.status_code, 200)
        self.auth["Authorization"] = f"Bearer {response.json()['access_token']}"
        await self.connect()
        self.assertEqual(len((await self.rpc("tools/list"))["tools"]), 17)
        response = await self.http.post("/oauth/token", data=exchange)
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json()["error"], "invalid_grant")


if __name__ == "__main__":
    unittest.main()
