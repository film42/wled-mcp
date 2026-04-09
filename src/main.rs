mod auth;
mod discovery;
mod models;
mod router;
mod server;
mod wled;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use crate::auth::{OAuthConfig, OAuthStore};
use crate::router::AuthMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wled_mcp=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .init();

    let bind_addr: SocketAddr = std::env::var("BIND_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()
        .expect("BIND_ADDRESS must be a valid socket address (e.g. 0.0.0.0:8080)");

    let auth_type = std::env::var("AUTH_TYPE")
        .unwrap_or_else(|_| "public".to_string())
        .to_lowercase();

    let auth_mode = match auth_type.as_str() {
        "oauth" => {
            let client_id = std::env::var("OAUTH_CLIENT_ID")
                .expect("OAUTH_CLIENT_ID is required when AUTH_TYPE=oauth");
            let client_secret = std::env::var("OAUTH_CLIENT_SECRET")
                .expect("OAUTH_CLIENT_SECRET is required when AUTH_TYPE=oauth");

            let allowed_redirect_uris: Vec<String> = std::env::var("OAUTH_ALLOWED_REDIRECT_URIS")
                .unwrap_or_else(|_| {
                    "https://chatgpt.com/connector/oauth/*,https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback".to_string()
                })
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let config = OAuthConfig {
                client_id: client_id.clone(),
                client_secret,
                allowed_redirect_uris,
            };

            tracing::info!("auth mode: oauth (client_id={})", client_id);
            AuthMode::OAuth(Arc::new(OAuthStore::new(config)))
        }
        "public" | "" => {
            tracing::info!("auth mode: public (no authentication)");
            AuthMode::Public
        }
        other => {
            anyhow::bail!(
                "unknown AUTH_TYPE '{}' — must be 'public' or 'oauth'",
                other
            );
        }
    };

    // Start mDNS discovery immediately so controllers are ready before first request
    let wled_client = Arc::new(wled::WledClient::new());
    let registry = Arc::new(discovery::ControllerRegistry::new(wled_client.clone()));
    tracing::info!("mDNS discovery started");

    // Create the MCP streamable HTTP service
    let config = StreamableHttpServerConfig::default();
    let mcp_service: StreamableHttpService<server::WledServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(server::WledServer::with_shared(
                    registry.clone(),
                    wled_client.clone(),
                ))
            },
            LocalSessionManager::default().into(),
            config,
        );

    let mcp_router = Router::new().nest_service("/mcp", mcp_service);
    let app = router::build(auth_mode, mcp_router);

    tracing::info!("WLED MCP server listening on {}", bind_addr);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down");
        })
        .await?;

    Ok(())
}
