"""Configuration loaded from the environment (single source of truth: ``.env``).

Nothing here needs a Hevy key or a secret at import time — only actual use does —
so the app boots and the health check answers even before it is fully configured.
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from functools import lru_cache

from dotenv import load_dotenv

# Load .env once at import. In Docker the file is bind-mounted, so every process
# start reads it fresh.
load_dotenv()

DEFAULT_REDIRECT_URIS: tuple[str, ...] = ("https://claude.ai/api/mcp/auth_callback",)
DEFAULT_HEVY_BASE_URL = "https://api.hevyapp.com"


def _env_bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def _env_csv(name: str, default: tuple[str, ...]) -> tuple[str, ...]:
    raw = os.getenv(name)
    if not raw:
        return default
    return tuple(part.strip() for part in raw.split(",") if part.strip())


@dataclass(frozen=True)
class Config:
    # ── Hevy API ──────────────────────────────────────────────────────────────
    hevy_api_key: str
    hevy_base_url: str

    # ── Single-user gate on the OAuth consent page ────────────────────────────
    # Plaintext, constant-time compared. It guards only the browser consent step;
    # the actual token exchange additionally requires the client secret. Behind
    # TLS this is adequate for a personal, single-user connector.
    auth_password: str

    # ── Token signing / cookies ───────────────────────────────────────────────
    session_secret: str
    cookie_secure: bool = True
    cookie_samesite: str = "lax"
    session_ttl: int = 60 * 60 * 24 * 30  # 30 days for the browser session
    token_ttl: int = 60 * 60 * 24 * 365   # 1 year for the MCP access token

    # ── OAuth client (Claude.ai) ──────────────────────────────────────────────
    mcp_client_id: str = "hevy-claude-connector"
    mcp_client_secret: str = ""
    mcp_redirect_uris: tuple[str, ...] = field(default_factory=lambda: DEFAULT_REDIRECT_URIS)


@lru_cache(maxsize=1)
def load_config() -> Config:
    session_secret = os.getenv("HEVY_MCP_SESSION_SECRET", "")
    if not session_secret:
        # Fail loud rather than silently signing tokens with an empty key.
        raise RuntimeError(
            "HEVY_MCP_SESSION_SECRET is not set — generate one with "
            "`python -c \"import secrets; print(secrets.token_urlsafe(48))\"`"
        )
    return Config(
        hevy_api_key=os.getenv("HEVY_API_KEY", ""),
        hevy_base_url=os.getenv("HEVY_BASE_URL", DEFAULT_HEVY_BASE_URL),
        auth_password=os.getenv("HEVY_MCP_PASSWORD", ""),
        session_secret=session_secret,
        cookie_secure=_env_bool("HEVY_MCP_COOKIE_SECURE", True),
        cookie_samesite=os.getenv("HEVY_MCP_COOKIE_SAMESITE", "lax"),
        mcp_client_id=os.getenv("HEVY_MCP_CLIENT_ID", "hevy-claude-connector"),
        mcp_client_secret=os.getenv("HEVY_MCP_CLIENT_SECRET", ""),
        mcp_redirect_uris=_env_csv("HEVY_MCP_REDIRECT_URIS", DEFAULT_REDIRECT_URIS),
    )
