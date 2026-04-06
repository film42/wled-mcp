mod auth;
mod discovery;
mod models;
mod server;
mod wled;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use tower_http::cors::{Any, CorsLayer};

use crate::auth::{OAuthConfig, OAuthStore};

#[derive(Clone, Debug)]
enum AuthMode {
    Public,
    OAuth(Arc<OAuthStore>),
}

#[derive(Clone, Debug)]
struct AppState {
    auth_mode: AuthMode,
}

fn landing_page(auth_mode: &AuthMode, base: &str) -> String {
    let mode_label = match auth_mode {
        AuthMode::Public => "Public (no authentication)",
        AuthMode::OAuth(_) => "OAuth (client_id / client_secret)",
    };

    let oauth_note = match auth_mode {
        AuthMode::Public => String::new(),
        AuthMode::OAuth(_) => format!(
            r#"<p>OAuth metadata: <code><a href="{base}/.well-known/oauth-authorization-server">{base}/.well-known/oauth-authorization-server</a></code></p>"#,
            base = base
        ),
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>WLED MCP Server</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 640px; margin: 60px auto; padding: 0 20px; color: #e0e0e0; background: #1a1a2e; }}
  h1 {{ color: #ff6b35; }}
  code {{ background: #16213e; padding: 2px 6px; border-radius: 4px; font-size: 0.95em; }}
  a {{ color: #4fc3f7; }}
  .mode {{ display: inline-block; padding: 4px 12px; border-radius: 4px; font-weight: 600; }}
  .mode-public {{ background: #2e7d32; color: #fff; }}
  .mode-oauth {{ background: #e65100; color: #fff; }}
  hr {{ border: none; border-top: 1px solid #333; margin: 24px 0; }}
</style>
</head>
<body>
  <h1>WLED MCP Server</h1>
  <p>Model Context Protocol server for controlling <a href="https://kno.wled.ge/">WLED</a> LED controllers.</p>
  <hr>
  <p><strong>Auth mode:</strong> <span class="mode {mode_class}">{mode_label}</span></p>
  <p><strong>MCP endpoint:</strong> <code>{base}/mcp</code></p>
  {oauth_note}
  <hr>
  <p>Point your MCP client (e.g. Claude) at:</p>
  <pre><code>{base}/mcp</code></pre>
</body>
</html>"#,
        mode_label = mode_label,
        mode_class = match auth_mode {
            AuthMode::Public => "mode-public",
            AuthMode::OAuth(_) => "mode-oauth",
        },
        base = base,
        oauth_note = oauth_note,
    )
}

async fn log_request(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let is_mcp_post = method == axum::http::Method::POST && uri.path() == "/mcp";

    if !is_mcp_post {
        let response = next.run(request).await;
        tracing::info!("{} {} {}", method, uri, response.status().as_u16());
        return response;
    }

    // Buffer MCP POST body to log the JSON-RPC method + tool name
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            let request = Request::from_parts(parts, Body::empty());
            let response = next.run(request).await;
            tracing::info!("POST /mcp {}", response.status().as_u16());
            return response;
        }
    };

    let rpc_label = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            let rpc_method = v.get("method")?.as_str()?;
            if rpc_method == "tools/call" {
                let tool = v.get("params")?.get("name")?.as_str().unwrap_or("?");
                let args = v.get("params").and_then(|p| p.get("arguments"));
                match args {
                    Some(a) => Some(format!("tools/call {} {}", tool, a)),
                    None => Some(format!("tools/call {}", tool)),
                }
            } else {
                Some(rpc_method.to_string())
            }
        });

    let request = Request::from_parts(parts, Body::from(bytes));
    let response = next.run(request).await;
    let status = response.status().as_u16();

    match rpc_label {
        Some(label) => tracing::info!("POST /mcp {} [{}]", status, label),
        None => tracing::info!("POST /mcp {}", status),
    }

    response
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let base = auth::base_url_from_headers(&headers);
    Html(landing_page(&state.auth_mode, &base))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wled_mcp=info".parse().unwrap()),
        )
        .init();

    let bind_addr: SocketAddr = std::env::var("BIND_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()
        .expect("BIND_ADDRESS must be a valid socket address (e.g. 0.0.0.0:3000)");

    let auth_type = std::env::var("AUTH_TYPE")
        .unwrap_or_else(|_| "public".to_string())
        .to_lowercase();

    let auth_mode = match auth_type.as_str() {
        "oauth" => {
            let client_id =
                std::env::var("OAUTH_CLIENT_ID").expect("OAUTH_CLIENT_ID is required when AUTH_TYPE=oauth");
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

    let app_state = AppState {
        auth_mode: auth_mode.clone(),
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

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = match auth_mode {
        AuthMode::Public => Router::new()
            .route("/", get(index))
            .nest_service("/mcp", mcp_service)
            .layer(cors)
            .with_state(app_state),
        AuthMode::OAuth(ref store) => {
            let store = store.clone();

            // Protected MCP route with auth middleware
            let protected_mcp = Router::new().nest_service("/mcp", mcp_service).layer(
                middleware::from_fn_with_state(store.clone(), auth::auth_middleware),
            );

            // OAuth endpoints that need CORS
            let oauth_cors_routes = Router::new()
                .route(
                    "/.well-known/oauth-authorization-server",
                    get(auth::oauth_metadata),
                )
                .route(
                    "/.well-known/oauth-protected-resource",
                    get(auth::protected_resource_metadata),
                )
                .route("/oauth/token", post(auth::oauth_token))
                .layer(cors)
                .with_state(store.clone());

            // Authorize needs its own state since it redirects (no CORS needed)
            let authorize_route = Router::new()
                .route("/oauth/authorize", get(auth::oauth_authorize))
                .with_state(store.clone());

            Router::new()
                .route("/", get(index))
                .with_state(app_state)
                .merge(authorize_route)
                .merge(oauth_cors_routes)
                .merge(protected_mcp)
        }
    }
    .layer(middleware::from_fn(log_request));

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
