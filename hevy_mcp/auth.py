"""Single-user session auth for the OAuth consent page + token signing.

Two ``itsdangerous`` serializers derive from the same secret under different
salts, so a browser session cookie and an MCP access token can never be replayed
as one another (signature verification fails across salts).
"""
from __future__ import annotations

import logging
import secrets
from typing import Optional
from urllib.parse import urlsplit

from fastapi import APIRouter, Form, Request, status
from fastapi.responses import HTMLResponse, RedirectResponse, Response
from itsdangerous import BadSignature, SignatureExpired, URLSafeTimedSerializer

from hevy_mcp.config import load_config
from hevy_mcp.templating import templates

logger = logging.getLogger(__name__)

SESSION_COOKIE = "hevy_mcp_session"

router = APIRouter()


def _get_serializer() -> URLSafeTimedSerializer:
    cfg = load_config()
    return URLSafeTimedSerializer(cfg.session_secret, salt="hevy-mcp-session")


def get_mcp_serializer() -> URLSafeTimedSerializer:
    cfg = load_config()
    return URLSafeTimedSerializer(cfg.session_secret, salt="hevy-mcp-token")


def safe_next(next: str | None) -> str:
    """Confine the post-login redirect to a same-site path (open-redirect guard)."""
    if not next or not next.startswith("/") or next.startswith("//") or "\\" in next:
        return "/"
    parsed = urlsplit(next)
    if parsed.scheme or parsed.netloc:
        return "/"
    return next


def read_session(token: str | None) -> Optional[str]:
    """Return the marker string if the session token is valid, else None."""
    if not token:
        return None
    cfg = load_config()
    try:
        payload = _get_serializer().loads(token, max_age=cfg.session_ttl)
    except (SignatureExpired, BadSignature):
        return None
    if not isinstance(payload, str):
        return None
    return payload


def create_session() -> str:
    return _get_serializer().dumps("owner")


def set_session_cookie(response: Response, token: str) -> None:
    cfg = load_config()
    response.set_cookie(
        key=SESSION_COOKIE,
        value=token,
        max_age=cfg.session_ttl,
        expires=cfg.session_ttl,
        path="/",
        secure=cfg.cookie_secure,
        httponly=True,
        samesite=cfg.cookie_samesite,
    )


def clear_session_cookie(response: Response) -> None:
    response.delete_cookie(key=SESSION_COOKIE, path="/")


def authenticate(password: str) -> bool:
    cfg = load_config()
    if not cfg.auth_password:
        return False
    return secrets.compare_digest(password, cfg.auth_password)


# ── Endpoints ────────────────────────────────────────────────────────────────
@router.get("/login", response_class=HTMLResponse)
async def login_page(request: Request, next: Optional[str] = None):
    token = request.cookies.get(SESSION_COOKIE)
    if read_session(token) is not None:
        return RedirectResponse(url=safe_next(next), status_code=status.HTTP_302_FOUND)
    return templates.TemplateResponse(request, "login.html", {"next": safe_next(next)})


@router.post("/login")
async def login(
    request: Request,
    password: str = Form(...),
    next: Optional[str] = Form(None),
):
    next_url = safe_next(next)
    if authenticate(password):
        response = RedirectResponse(url=next_url, status_code=status.HTTP_303_SEE_OTHER)
        set_session_cookie(response, create_session())
        return response
    return templates.TemplateResponse(
        request,
        "login.html",
        {"error": "Неверный пароль", "next": next_url},
        status_code=status.HTTP_401_UNAUTHORIZED,
    )


@router.get("/logout")
@router.post("/logout")
async def logout():
    response = RedirectResponse(url="/login", status_code=status.HTTP_303_SEE_OTHER)
    clear_session_cookie(response)
    return response
