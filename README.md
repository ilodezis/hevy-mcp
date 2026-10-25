# hevy-mcp

A remote [MCP](https://modelcontextprotocol.io) server that connects Claude to your [Hevy](https://www.hevyapp.com) workout log. Claude can read your training history, look up exercises, build routines and log finished workouts, all from a normal conversation.

It is a single-user connector: one Hevy account, one password, no database. Hevy is the only datastore. The server is one static Rust binary (axum for HTTP, [`rmcp`](https://crates.io/crates/rmcp) for the MCP protocol) that idles at a few MB of RAM and ships as a `FROM scratch` container image of about 7 MB.

## What it looks like in use

Once the connector is added to Claude.ai you can ask things like:

- "How has my bench press moved over the last six months? Any plateaus?"
- "Build me a 4-day upper/lower routine from what I actually lift, and put it in a new folder."
- "Log yesterday's session: squats 5x5 at 100 kg, then 3 sets of leg curls."
- "What was my best set of deadlifts ever, and when?"

Behind those, Claude calls the tools listed [below](#tools). A typical progress question is two calls: `search_exercise_templates` to find the exercise id, then `get_exercise_history` for every set ever logged on it.

## How it works

```
Claude.ai ──HTTPS──▶ reverse proxy ──▶ hevy-mcp ──HTTPS──▶ api.hevyapp.com
                     (TLS, Host,        │  ├─ /login, /oauth/*   single-user OAuth server
                      X-Forwarded-      │  └─ /mcp/              MCP, behind a Bearer token
                      Proto)            └─ holds your Hevy API key; nothing is stored on disk
```

Claude.ai only talks to MCP servers that speak OAuth 2.0, so the server includes a small authorization server. When you add the connector, Claude sends you to a login page on your own server, you enter the password you configured, approve the access, and Claude receives a signed access token. Every request to `/mcp/` must carry that token. The Hevy API key never leaves the server.

## Requirements

- A **Hevy PRO** subscription. Hevy's public API, and so this connector, is a PRO feature.
- A Hevy API key from the [developer settings](https://hevy.com/settings?developer). The API itself is documented at <https://api.hevyapp.com/docs/>.
- For use with Claude.ai: a host with a public HTTPS URL. Claude connects from Anthropic's side, so `localhost` will not work. Any VPS with Docker and a reverse proxy is enough.
- To build from source: Rust 1.88 or newer (edition 2024).

## Quick start

```bash
git clone https://github.com/ilodezis/hevy-mcp.git
cd hevy-mcp
cp .env.example .env
```

Edit `.env`. At minimum set the Hevy key and replace every `change_me_*` value:

```bash
openssl rand -hex 32   # run it once per secret
```

For a local run over plain HTTP, also set `HEVY_MCP_COOKIE_SECURE=false` and `HEVY_MCP_BIND=127.0.0.1:8010`, then:

```bash
cargo run --release
```

Check that it is alive:

```bash
curl http://127.0.0.1:8010/
# {"service":"hevy-mcp","status":"ok","hevy_configured":true}
```

`hevy_configured` is `false` when `HEVY_API_KEY` is empty. The server still starts, but every tool call returns an error explaining that the key is missing.

## Configuration

Settings come from environment variables. On start the server also loads the first `.env` it finds, walking up from the working directory; variables already present in the environment win over the file.

| Variable | Default | Notes |
|---|---|---|
| `HEVY_API_KEY` | empty | Your Hevy API key. Without it every tool returns an error. |
| `HEVY_BASE_URL` | `https://api.hevyapp.com` | Only change it to point at a mock or a proxy. |
| `HEVY_MCP_PASSWORD` | empty | The password for the login page. If empty, **nobody can log in**. |
| `HEVY_MCP_SESSION_SECRET` | none, required | Signs access tokens and the browser session cookie. The server refuses to start without it. |
| `HEVY_MCP_CLIENT_ID` | `hevy-claude-connector` | OAuth client id that Claude must present. |
| `HEVY_MCP_CLIENT_SECRET` | empty | OAuth client secret. If empty, the token endpoint rejects every request. |
| `HEVY_MCP_REDIRECT_URIS` | `https://claude.ai/api/mcp/auth_callback` | Comma-separated allow-list of OAuth redirect URIs. |
| `HEVY_MCP_COOKIE_SECURE` | `true` | Marks the session cookie `Secure`. Set `false` only for local HTTP testing. |
| `HEVY_MCP_COOKIE_SAMESITE` | `lax` | `strict`, `lax` or `none`. |
| `HEVY_MCP_BIND` | `0.0.0.0:8010` | Listen address. |

`.env` is parsed with python-dotenv semantics (quoting, inline `#` comments, escapes, `${VAR}` and `${VAR:-default}` expansion), so a `$` or `#` inside a password behaves the way you would expect from that format. Wrap such values in single quotes if in doubt.

The password and both secrets are plain strings you invent. The two secrets should be long and random; the password is the one thing you will type by hand.

## Connecting Claude.ai

1. Deploy the server behind HTTPS (see [Deployment](#deployment)).
2. In Claude.ai open **Settings → Connectors → Add custom connector**.
3. Set the URL to `https://your.host.example/mcp/`. The trailing slash matters; `/mcp` redirects to `/mcp/`.
4. In the connector's advanced settings enter the OAuth client id and secret, the values of `HEVY_MCP_CLIENT_ID` and `HEVY_MCP_CLIENT_SECRET`. The server has no dynamic client registration, so Claude needs them up front.
5. Claude opens the login page on your server. Enter `HEVY_MCP_PASSWORD`, then approve the access on the consent page.

The login and consent pages are written in Russian. They are two small templates in [`templates/`](templates) if you want to translate them.

## Tools

17 tools: 11 read, 6 write. Their names, descriptions and JSON schemas live in [`src/tools.json`](src/tools.json) and are served as is.

### Read

| Tool | Arguments | What it returns |
|---|---|---|
| `workout_count` | none | Total number of logged workouts. |
| `list_workouts` | `page`, `page_size` | Workouts, newest first. `page_size` is capped at 10 by Hevy. |
| `get_workout` | `workout_id` | One workout with its full exercise and set tree. |
| `list_routines` | `page`, `page_size` | Saved routines. `page_size` is capped at 10. |
| `get_routine` | `routine_id` | One routine with its exercises and sets. |
| `list_routine_folders` | `page`, `page_size` | Routine folders such as Push / Pull / Legs. |
| `get_routine_folder` | `folder_id` | One folder: title, position, timestamps. |
| `get_user_info` | none | `{id, name, url}` of the Hevy account the connector is wired to. |
| `search_exercise_templates` | `query`, `limit` (default 20) | Templates whose title contains `query`, case-insensitive. Scans up to about 1000 templates, built-in and custom. |
| `get_exercise_template` | `template_id` | One exercise template. |
| `get_exercise_history` | `exercise_template_id`, `start_date`, `end_date` | Every set ever performed for one exercise, each tagged with its workout. Dates are optional ISO-8601 timestamps. The response is not paginated, so pass `start_date` for exercises you have done for years. |

### Write

| Tool | Arguments | Effect |
|---|---|---|
| `create_routine` | `title`, `exercises`, `notes`, `folder_id` | Creates a routine. |
| `update_routine` | `routine_id`, `title`, `exercises`, `notes` | **Replaces** the routine. Read it with `get_routine` first and send the complete desired state. |
| `create_routine_folder` | `title` | Creates a folder. |
| `create_workout` | `title`, `start_time`, `end_time`, `exercises`, `description`, `is_private` | Logs a completed workout. |
| `update_workout` | `workout_id`, `title`, `start_time`, `end_time`, `exercises`, `description`, `is_private` | **Overwrites** a logged workout with the state you send. |
| `create_exercise_template` | `title`, `exercise_type`, `equipment_category`, `muscle_group`, `other_muscles` | Creates a custom exercise. Hevy may refuse this on some plans. |

An exercise slot is `{exercise_template_id, sets, superset_id?, rest_seconds?, notes?}`. A set carries `type` (`warmup`, `normal`, `failure` or `dropset`) plus whichever of `weight_kg`, `reps`, `distance_meters`, `duration_seconds` fits the exercise, and optionally `rpe` and `custom_metric`. Give exercises the same integer `superset_id` to group them into a superset. The exact schemas are in `src/tools.json`.

### Behaviour worth knowing

- **Nothing can be deleted.** Hevy's public API has no delete endpoints, so there is no delete tool. Claude can overwrite a workout or routine but not remove one.
- **Errors are data, not crashes.** A bad id, a missing API key or a Hevy 4xx comes back as `{"error": "..."}` so the model can read it and recover. Hevy error bodies are cut to 400 characters. Calls to Hevy time out after 30 seconds.
- **Arguments are validated before anything reaches Hevy**, with coercion in the style of pydantic: `"2"` becomes `2`, `"yes"` becomes `true`, unknown arguments are rejected. Failures return pydantic's error report verbatim, and models have learned to read it.
- **Both MCP revisions are served:** the session-based streamable HTTP transport (responses over SSE) and the stateless `2026-07-28` revision (plain JSON).

## Security model

This is built for one person, and the design leans on that.

- **Login:** a single password, compared in constant time. If `HEVY_MCP_PASSWORD` is empty, login is impossible rather than open. There is no rate limiting on the login form, so pick a long password and keep the server behind your proxy.
- **Session cookie:** `HttpOnly`, `Secure` by default, `SameSite=lax` by default, valid for 30 days.
- **Authorization codes:** random, single use, valid for 5 minutes, held in memory. PKCE is supported (`S256` and `plain`) and enforced whenever Claude sends a challenge. The post-login redirect only accepts same-site paths.
- **Access tokens:** HMAC-signed with a timestamp, valid for one year, accepted only in the `Authorization: Bearer` header, never in a query string. The browser cookie and the access token use different salts, so one cannot be replayed as the other.
- **Revocation:** tokens are stateless, so there is no per-token revoke. Rotating `HEVY_MCP_SESSION_SECRET` invalidates every token and session at once; Claude then has to reconnect. Changing `HEVY_MCP_CLIENT_ID` does the same.
- **Secrets stay server-side:** the Hevy API key is only ever sent to Hevy.
- **Limits:** login and OAuth form bodies are capped at 1 MB.

Never expose the container port directly. Bind it to loopback and put a TLS-terminating proxy in front.

## Deployment

The image is `FROM scratch`: a statically linked musl binary and nothing else. TLS roots are compiled in, so no CA bundle is needed in the image.

```bash
cp .env.example .env     # then fill it in; keep HEVY_MCP_COOKIE_SECURE=true
docker compose up -d --build
```

[`docker-compose.yml`](docker-compose.yml) publishes `127.0.0.1:8010` only and bind-mounts `.env` read-only, so the file is read fresh on every start. A change to `.env` needs `docker compose restart`, not a rebuild.

The release build peaks around 1 GB of RAM because of fat LTO. On a small VPS, build with one job (about 650 MB):

```bash
docker compose build --build-arg CARGO_BUILD_JOBS=1
```

To update an existing checkout, [`deploy.sh`](deploy.sh) fetches, hard-resets to `origin/master` (discarding local edits to tracked files) and rebuilds:

```bash
./deploy.sh
```

### Reverse proxy

The proxy must terminate TLS and pass the original `Host` and `X-Forwarded-Proto`; the server builds its OAuth URLs from them, so without those headers it advertises `http://` addresses that Claude will refuse.

Caddy sets both headers by default:

```caddy
mcp.example.com {
    reverse_proxy 127.0.0.1:8010
}
```

nginx:

```nginx
server {
    listen 443 ssl;
    server_name mcp.example.com;

    ssl_certificate     /path/to/fullchain.pem;
    ssl_certificate_key /path/to/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8010;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # MCP sessions stream over SSE; do not buffer them.
        proxy_buffering off;
        proxy_read_timeout 1h;
    }
}
```

### Restarts

Access tokens are stateless and survive restarts. MCP sessions live in memory: after a restart, clients get "session not found" and re-initialize on their own. Idle sessions are dropped after 24 hours anyway.

## HTTP endpoints

| Path | Purpose |
|---|---|
| `GET /` | Health check: `{"service", "status", "hevy_configured"}`. |
| `GET`, `POST /login` | Password form and the session cookie. |
| `GET`, `POST /logout` | Clears the cookie. |
| `GET /.well-known/oauth-authorization-server` | OAuth metadata (RFC 8414). |
| `GET /oauth/authorize` | Consent page; sends you to `/login` first if there is no session. |
| `POST /oauth/authorize/approve` | Issues an authorization code. |
| `POST /oauth/token` | Exchanges the code for an access token. Accepts form or JSON bodies, and the client secret in the body or as HTTP Basic. |
| `/mcp/` | The MCP endpoint. Requires `Authorization: Bearer <token>`. |

## Development

```bash
cargo test
```

The suite has two layers:

- **Unit tests** in the modules: token signing, `.env` parsing, argument validation, URL quoting, the open-redirect guard, PKCE.
- **End-to-end tests** in [`tests/integration.rs`](tests/integration.rs). They start the real binary against an in-process mock of the Hevy API and drive it over HTTP: all 17 tools over both MCP revisions, the login and PKCE token exchange (including single-use codes), argument validation, upstream error handling, and the token and cookie formats. They use throwaway credentials and never touch a real account.

Source layout:

| Path | Role |
|---|---|
| `src/main.rs` | Startup, listener, MCP service wiring, graceful shutdown. |
| `src/web.rs` | Routing, login gate, OAuth authorization server, Bearer check in front of `/mcp/`. |
| `src/mcp.rs` | The rmcp handler: serves `tools.json` and dispatches tool calls. |
| `src/validate.rs` | Argument checking derived from the schemas in `tools.json`. |
| `src/hevy.rs` | Hevy REST client (hyper + rustls). |
| `src/signing.rs` | Signed timed tokens. |
| `src/config.rs` | Environment and `.env` loading. |
| `src/urlenc.rs` | URL quoting rules the wire format depends on. |
| `src/templates.rs`, `templates/` | Login and consent pages. |

## Compatibility with the earlier Python server

This project replaced a Python (FastMCP) implementation, and the Claude.ai connection created against that server had to keep working. The wire contract is therefore frozen, and a few things look odd without that context:

- `src/tools.json` is the verbatim `tools/list` of the old server. Renaming a tool or editing a description or schema changes what Claude sees; treat it as a product decision.
- Access tokens and the session cookie use the [itsdangerous](https://itsdangerous.palletsprojects.com) URL-safe timed format (HMAC-SHA1 over a key derived from the secret and a salt, salts `hevy-mcp-token` and `hevy-mcp-session`). Changing the format, a salt or the secret logs Claude out and the connector must be added again.
- Routing reproduces the old framework's observable behaviour: JSON `404` and `405` bodies, `422` for missing form fields, a `307` from `/mcp` to `/mcp/`.

If you are starting fresh none of this constrains you, but changing any of it breaks existing installs.

## Limitations

- One user and one Hevy account per deployment.
- No deletes, because Hevy's API has none.
- Whether `create_exercise_template` works depends on your Hevy plan.
- Login and consent pages are in Russian only.
- There is no way to revoke a single token, only all of them.

## License

MIT. See [LICENSE](LICENSE).
