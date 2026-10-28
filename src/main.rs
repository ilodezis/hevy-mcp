//! Hevy MCP — a standalone remote Model Context Protocol connector for Claude.ai.
//!
//! Wraps the Hevy public REST API (https://api.hevyapp.com/docs/) as MCP tools
//! so Claude can read and *build* workouts and routines in a conversation.
//! Single user, no database — Hevy itself is the datastore.
//!
//! This server replaced an earlier Python (FastMCP) one and keeps its wire
//! contract: tool names and schemas, the OAuth handshake and consent pages,
//! and the token format, so existing Claude connections keep working.

mod config;
mod hevy;
mod mcp;
mod signing;
mod templates;
mod urlenc;
mod validate;
mod web;

use std::{env, net::SocketAddr, sync::Arc, time::Duration};

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{config::Config, hevy::HevyClient, mcp::HevyMcp, web::App};

/// Idle MCP sessions are reaped after a day. FastMCP kept them until restart;
/// clients already re-initialize on "session not found" after every deploy.
const SESSION_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

fn main() {
    config::load_dotenv();
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("ERROR: {message}");
            std::process::exit(1);
        }
    };
    let bind: SocketAddr = match env::var("HEVY_MCP_BIND").as_deref().unwrap_or("0.0.0.0:8010").parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("ERROR: HEVY_MCP_BIND: {e}");
            std::process::exit(1);
        }
    };

    // One thread is plenty for a single-user connector and keeps memory flat.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(serve(cfg, bind));
}

async fn serve(cfg: Config, bind: SocketAddr) {
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("ERROR: cannot listen on {bind}: {e}");
            std::process::exit(1);
        }
    };
    let local_addr = listener.local_addr().expect("bound address");

    let hevy = Arc::new(HevyClient::new(cfg.hevy_base_url.clone(), cfg.hevy_api_key.clone()));
    let shutdown = CancellationToken::new();
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(SESSION_IDLE);
    let mcp_config = StreamableHttpServerConfig::default()
        // The reverse proxy forwards the public Host; public host validation
        // would require new configuration not present in existing deployments.
        .disable_allowed_hosts()
        .with_cancellation_token(shutdown.child_token());
    let mcp = StreamableHttpService::new(
        {
            let hevy = hevy.clone();
            move || Ok(HevyMcp::new(hevy.clone()))
        },
        Arc::new(sessions),
        mcp_config,
    );

    let app = Arc::new(App::new(cfg, hevy, mcp, local_addr));
    let router = axum::Router::new().fallback(web::handle).with_state(app);

    eprintln!("INFO: hevy-mcp {} listening on http://{local_addr}", env!("CARGO_PKG_VERSION"));
    let result = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            eprintln!("INFO: shutting down");
            // End open SSE streams so the graceful shutdown can finish.
            shutdown.cancel();
        })
        .await;
    if let Err(e) = result {
        eprintln!("ERROR: server: {e}");
        std::process::exit(1);
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
