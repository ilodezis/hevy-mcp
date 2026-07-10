"""Local dev entrypoint: `python run_local.py` → http://127.0.0.1:8010

For local testing set HEVY_MCP_COOKIE_SECURE=false (no HTTPS on localhost).
"""
from __future__ import annotations

import uvicorn

if __name__ == "__main__":
    uvicorn.run("hevy_mcp.app:app", host="127.0.0.1", port=8010, reload=True)
