"""FastAPI application: OAuth + login routers, and the MCP app mounted at /mcp
behind a Bearer-token ASGI middleware.
"""
from __future__ import annotations

import logging
from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.responses import JSONResponse

from hevy_mcp.auth import get_mcp_serializer
from hevy_mcp.auth import router as auth_router
from hevy_mcp.config import load_config
from hevy_mcp.oauth import router as oauth_router
from hevy_mcp.server import mcp

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


class MCPAuthMiddleware:
    """ASGI middleware guarding the mounted MCP app with the signed Bearer token."""

    def __init__(self, app, client_id: str, max_age: int):
        self.app = app
        self.client_id = client_id
        self.max_age = max_age

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        if scope.get("method") == "OPTIONS":
            await send({
                "type": "http.response.start",
                "status": 200,
                "headers": [
                    (b"access-control-allow-origin", b"*"),
                    (b"access-control-allow-methods", b"GET, POST, OPTIONS"),
                    (b"access-control-allow-headers", b"Authorization, Content-Type"),
                    (b"content-length", b"0"),
                ],
            })
            await send({"type": "http.response.body", "body": b"", "more_body": False})
            return

        headers = dict(scope.get("headers", []))
        auth_header = headers.get(b"authorization", b"").decode("utf-8")

        # Bearer header ONLY — never a query param (leaks into logs/history).
        token = auth_header[7:] if auth_header.lower().startswith("bearer ") else None

        authenticated = False
        if token:
            from itsdangerous import BadSignature, SignatureExpired
            try:
                payload = get_mcp_serializer().loads(token, max_age=self.max_age)
                if (
                    isinstance(payload, dict)
                    and payload.get("type") == "mcp_access_token"
                    and payload.get("client_id") == self.client_id
                ):
                    authenticated = True
            except (SignatureExpired, BadSignature):
                pass

        if not authenticated:
            body = b'{"detail":"Unauthorized. Invalid or missing MCP access token."}'
            await send({
                "type": "http.response.start",
                "status": 401,
                "headers": [
                    (b"content-type", b"application/json"),
                    (b"content-length", str(len(body)).encode("utf-8")),
                    (b"www-authenticate", b"Bearer"),
                ],
            })
            await send({"type": "http.response.body", "body": body, "more_body": False})
            return

        try:
            await self.app(scope, receive, send)
        except TypeError:
            logger.exception("MCP app raised TypeError handling %s", scope.get("path"))


def create_app() -> FastAPI:
    cfg = load_config()

    # Streamable HTTP — the SSE transport is deprecated in the MCP spec since
    # 2025-03. path="/" so that mounting on /mcp lands the endpoint on /mcp/
    # rather than /mcp/mcp (the library's own default path would be appended).
    mcp_app = mcp.http_app(transport="http", path="/")

    @asynccontextmanager
    async def lifespan(_app: FastAPI):
        # Streamable HTTP builds its session manager inside the mounted app's
        # lifespan, and app.mount() never runs a sub-app's lifespan — without
        # this every /mcp/ request fails with "manager not initialized".
        async with mcp_app.router.lifespan_context(_app):
            yield

    app = FastAPI(
        title="Hevy MCP Connector", docs_url=None, redoc_url=None, lifespan=lifespan,
    )

    app.include_router(auth_router)
    app.include_router(oauth_router)

    @app.get("/")
    async def health():
        from hevy_mcp.client import HevyClient
        return JSONResponse({
            "service": "hevy-mcp",
            "status": "ok",
            "hevy_configured": HevyClient.from_config().is_configured,
        })

    app.mount("/mcp", MCPAuthMiddleware(
        mcp_app, client_id=cfg.mcp_client_id, max_age=cfg.token_ttl,
    ))

    return app


app = create_app()
