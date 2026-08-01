"""Smoke check for the MCP endpoint — `python check_mcp.py`, no server needed.

Drives the real ASGI app in-process to cover the three things that break when
the transport or the mount changes:
  1. the streamable-HTTP session manager is actually started (mounted sub-apps
     do not get their lifespan run unless the parent enters it),
  2. the Bearer middleware still refuses missing/forged tokens,
  3. the endpoint answers on /mcp/ and advertises the full tool list.

Runs against a throwaway signing secret, so it needs no .env and never touches
Hevy.
"""
from __future__ import annotations

import asyncio
import json
import os
import sys

os.environ.setdefault("HEVY_MCP_SESSION_SECRET", "check-mcp-throwaway-secret")

import httpx  # noqa: E402

from hevy_mcp.app import app  # noqa: E402
from hevy_mcp.auth import get_mcp_serializer  # noqa: E402
from hevy_mcp.config import load_config  # noqa: E402

MCP_PATH = "/mcp/"
INIT = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "check_mcp", "version": "0"},
    },
}
ACCEPT = "application/json, text/event-stream"


def _sse_payload(body: str) -> dict:
    """Pull the JSON out of a text/event-stream response (one `data:` line)."""
    for line in body.splitlines():
        if line.startswith("data:"):
            return json.loads(line[5:].strip())
    return json.loads(body)


async def main() -> int:
    token = get_mcp_serializer().dumps(
        {"type": "mcp_access_token", "client_id": load_config().mcp_client_id}
    )
    transport = httpx.ASGITransport(app=app)

    # The mounted MCP app's session manager lives in the parent lifespan.
    async with app.router.lifespan_context(app):
        async with httpx.AsyncClient(
            transport=transport, base_url="http://check"
        ) as http:
            for label, headers in (
                ("no token ", {}),
                ("bad token", {"Authorization": "Bearer garbage"}),
            ):
                r = await http.post(
                    MCP_PATH, json=INIT, headers={"Accept": ACCEPT, **headers}
                )
                assert r.status_code == 401, f"{label}: got {r.status_code}, want 401"
                print(f"[ok] {label} -> 401")

            auth = {"Authorization": f"Bearer {token}", "Accept": ACCEPT}
            r = await http.post(MCP_PATH, json=INIT, headers=auth)
            assert r.status_code == 200, f"initialize: {r.status_code} {r.text[:200]}"
            info = _sse_payload(r.text)["result"]["serverInfo"]
            session = r.headers["mcp-session-id"]
            print(f"[ok] initialize -> {info['name']} (session {session[:8]}…)")

            auth["mcp-session-id"] = session
            await http.post(
                MCP_PATH,
                json={"jsonrpc": "2.0", "method": "notifications/initialized"},
                headers=auth,
            )
            r = await http.post(
                MCP_PATH,
                json={"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
                headers=auth,
            )
            tools = sorted(t["name"] for t in _sse_payload(r.text)["result"]["tools"])
            assert len(tools) == 14, f"expected 14 tools, got {len(tools)}: {tools}"
            assert "create_routine" in tools and "workout_count" in tools, tools
            print(f"[ok] tools/list -> {len(tools)} tools: {', '.join(tools)}")

    print("\nALL CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
