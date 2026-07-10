"""OAuth 2.0 Authorization Server for the Claude.ai connector handshake.

Implements RFC 8414 metadata discovery, a single-user consent page, and the
authorization-code → Bearer-token exchange with PKCE. Authorization codes live
in an in-process TTL store (single worker, single user — no Redis needed).
"""
from __future__ import annotations

import base64
import hashlib
import json
import logging
import secrets
import time
from typing import Optional
from urllib.parse import urlencode, urlsplit

from fastapi import APIRouter, Form, HTTPException, Request, status
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse

from hevy_mcp.auth import SESSION_COOKIE, get_mcp_serializer, read_session
from hevy_mcp.config import load_config
from hevy_mcp.templating import templates

logger = logging.getLogger(__name__)

router = APIRouter(tags=["oauth"])

# ── In-process authorization-code store (code → payload, 5-minute TTL) ─────────
_CODE_TTL = 300
_codes: dict[str, tuple[float, dict]] = {}


def _store_code(code: str, payload: dict) -> None:
    now = time.time()
    # Opportunistic sweep of expired codes.
    for k, (exp, _) in list(_codes.items()):
        if exp <= now:
            _codes.pop(k, None)
    _codes[code] = (now + _CODE_TTL, payload)


def _pop_code(code: str) -> Optional[dict]:
    entry = _codes.pop(code, None)
    if entry is None:
        return None
    exp, payload = entry
    if exp <= time.time():
        return None
    return payload


def verify_pkce(code_verifier: str, code_challenge: str, method: Optional[str]) -> bool:
    if not method or method == "plain":
        return secrets.compare_digest(code_verifier, code_challenge)
    if method == "S256":
        digest = hashlib.sha256(code_verifier.encode("utf-8")).digest()
        calculated = base64.urlsafe_b64encode(digest).decode("utf-8").rstrip("=")
        return secrets.compare_digest(calculated, code_challenge.rstrip("="))
    return False


# ── Metadata discovery (RFC 8414) ─────────────────────────────────────────────
@router.get("/.well-known/oauth-authorization-server")
async def oauth_metadata(request: Request):
    base_url = str(request.base_url).rstrip("/")
    return {
        "issuer": base_url,
        "authorization_endpoint": f"{base_url}/oauth/authorize",
        "token_endpoint": f"{base_url}/oauth/token",
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "token_endpoint_auth_methods_supported": [
            "client_secret_post",
            "client_secret_basic",
        ],
        "code_challenge_methods_supported": ["S256", "plain"],
    }


# ── Authorization consent ─────────────────────────────────────────────────────
@router.get("/oauth/authorize", response_class=HTMLResponse)
async def oauth_authorize(
    request: Request,
    response_type: str,
    client_id: str,
    redirect_uri: str,
    state: Optional[str] = None,
    code_challenge: Optional[str] = None,
    code_challenge_method: Optional[str] = None,
):
    cfg = load_config()

    if client_id != cfg.mcp_client_id:
        return templates.TemplateResponse(
            request, "oauth_authorize.html",
            {"error": "Неизвестный client_id.", "redirect_uri": redirect_uri},
        )
    if response_type != "code":
        return templates.TemplateResponse(
            request, "oauth_authorize.html",
            {"error": "Поддерживается только response_type=code.", "redirect_uri": redirect_uri},
        )
    if redirect_uri not in cfg.mcp_redirect_uris:
        return templates.TemplateResponse(
            request, "oauth_authorize.html",
            {"error": "redirect_uri не в списке разрешённых.", "redirect_uri": redirect_uri},
        )

    # Require a logged-in owner session; otherwise bounce to login and come back.
    if read_session(request.cookies.get(SESSION_COOKIE)) is None:
        next_path = str(request.url.path)
        if request.url.query:
            next_path += f"?{request.url.query}"
        login_url = f"/login?{urlencode({'next': next_path})}"
        return RedirectResponse(url=login_url, status_code=status.HTTP_302_FOUND)

    return templates.TemplateResponse(
        request, "oauth_authorize.html",
        {
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "redirect_domain": urlsplit(redirect_uri).netloc,
            "state": state,
            "code_challenge": code_challenge,
            "code_challenge_method": code_challenge_method,
        },
    )


@router.post("/oauth/authorize/approve")
async def oauth_approve(
    request: Request,
    client_id: str = Form(...),
    redirect_uri: str = Form(...),
    state: Optional[str] = Form(None),
    code_challenge: Optional[str] = Form(None),
    code_challenge_method: Optional[str] = Form(None),
):
    if read_session(request.cookies.get(SESSION_COOKIE)) is None:
        raise HTTPException(status_code=401, detail="Not authenticated")

    cfg = load_config()
    if client_id != cfg.mcp_client_id:
        raise HTTPException(status_code=400, detail="Invalid client_id")
    if redirect_uri not in cfg.mcp_redirect_uris:
        raise HTTPException(status_code=400, detail="redirect_uri not allowed")

    code = f"code_{secrets.token_urlsafe(32)}"
    _store_code(code, {
        "client_id": client_id,
        "redirect_uri": redirect_uri,
        "code_challenge": code_challenge,
        "code_challenge_method": code_challenge_method,
    })

    params = {"code": code}
    if state:
        params["state"] = state
    separator = "&" if "?" in redirect_uri else "?"
    return RedirectResponse(
        url=f"{redirect_uri}{separator}{urlencode(params)}",
        status_code=status.HTTP_302_FOUND,
    )


# ── Token exchange ────────────────────────────────────────────────────────────
@router.post("/oauth/token")
async def oauth_token(request: Request):
    content_type = request.headers.get("content-type", "")
    if "application/json" in content_type:
        body = await request.json()
    else:
        body = dict(await request.form())

    grant_type = body.get("grant_type")
    code = body.get("code")
    redirect_uri = body.get("redirect_uri")
    client_id = body.get("client_id")
    client_secret = body.get("client_secret")
    code_verifier = body.get("code_verifier")

    cfg = load_config()

    if not client_secret:
        auth_header = request.headers.get("authorization", "")
        if auth_header.lower().startswith("basic "):
            try:
                decoded = base64.b64decode(auth_header[6:]).decode("utf-8")
                cid, csec = decoded.split(":", 1)
                client_id = client_id or cid
                client_secret = csec
            except Exception:
                pass

    if client_id != cfg.mcp_client_id:
        return JSONResponse(status_code=400, content={
            "error": "invalid_client", "error_description": "Client ID mismatch"})

    if not cfg.mcp_client_secret:
        return JSONResponse(status_code=400, content={
            "error": "invalid_client", "error_description": "Client secret not configured"})

    if not secrets.compare_digest(client_secret or "", cfg.mcp_client_secret):
        return JSONResponse(status_code=400, content={
            "error": "invalid_client", "error_description": "Client secret mismatch"})

    if grant_type != "authorization_code":
        return JSONResponse(status_code=400, content={
            "error": "unsupported_grant_type",
            "error_description": "Only authorization_code is supported"})

    if not code:
        return JSONResponse(status_code=400, content={
            "error": "invalid_request", "error_description": "Missing code"})

    code_data = _pop_code(code)  # single-use
    if code_data is None:
        return JSONResponse(status_code=400, content={
            "error": "invalid_grant", "error_description": "Code expired or invalid"})

    if redirect_uri not in cfg.mcp_redirect_uris or redirect_uri != code_data["redirect_uri"]:
        return JSONResponse(status_code=400, content={
            "error": "invalid_grant", "error_description": "Redirect URI mismatch"})

    stored_challenge = code_data.get("code_challenge")
    if stored_challenge:
        if not code_verifier:
            return JSONResponse(status_code=400, content={
                "error": "invalid_grant", "error_description": "Missing code_verifier"})
        if not verify_pkce(code_verifier, stored_challenge, code_data.get("code_challenge_method")):
            return JSONResponse(status_code=400, content={
                "error": "invalid_grant", "error_description": "PKCE verification failed"})

    access_token = get_mcp_serializer().dumps({
        "client_id": client_id,
        "type": "mcp_access_token",
    })

    return {
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": cfg.token_ttl,
    }
