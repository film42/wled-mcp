use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Request};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tower_http::cors::{Any, CorsLayer};

#[cfg(test)]
use crate::auth::OAuthConfig;
use crate::auth::{self, OAuthStore};

#[derive(Clone, Debug)]
pub enum AuthMode {
    Public,
    OAuth(Arc<OAuthStore>),
}

#[derive(Clone, Debug)]
struct AppState {
    auth_mode: AuthMode,
}

/// Build the complete application router.
///
/// `mcp_router` should be a Router that already has the MCP service nested at `/mcp`.
/// Pass `Router::new()` if you don't need the MCP service (e.g. in tests).
pub fn build(auth_mode: AuthMode, mcp_router: Router) -> Router {
    let app_state = AppState {
        auth_mode: auth_mode.clone(),
    };

    // Expose the MCP session headers so browser-based clients (e.g. MCP
    // Inspector) can read them from the initialize response and echo them
    // back on subsequent requests. Without this, the browser hides the
    // headers from JS and the server 422s on notifications/initialized.
    // See modelcontextprotocol/inspector#905.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            HeaderName::from_static("mcp-session-id"),
            HeaderName::from_static("mcp-protocol-version"),
        ]);

    let app = match auth_mode {
        AuthMode::Public => Router::new()
            .route("/", get(index))
            .with_state(app_state)
            .merge(mcp_router)
            .layer(cors),
        AuthMode::OAuth(ref store) => {
            let store = store.clone();

            let protected_mcp = mcp_router.layer(middleware::from_fn_with_state(
                store.clone(),
                auth::auth_middleware,
            ));

            let oauth_routes = Router::new()
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

            let authorize_route = Router::new()
                .route("/oauth/authorize", get(auth::oauth_authorize))
                .with_state(store.clone());

            Router::new()
                .route("/", get(index))
                .with_state(app_state)
                .merge(oauth_routes)
                .merge(authorize_route)
                .merge(protected_mcp)
        }
    };

    app.layer(middleware::from_fn(log_request))
}

/// Build an OAuth-mode router from config. Convenience for tests and simple setups.
#[cfg(test)]
pub fn build_oauth(config: OAuthConfig) -> Router {
    let store = Arc::new(OAuthStore::new(config));
    build(AuthMode::OAuth(store), Router::new())
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let base = auth::base_url_from_headers(&headers);
    Html(landing_page(&state.auth_mode, &base))
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

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::{Method, Request, StatusCode};
    use base64::Engine;
    use tower::ServiceExt;

    use super::*;
    use crate::auth::OAuthConfig;

    fn test_config() -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: "test-secret".to_string(),
            allowed_redirect_uris: vec![
                "https://claude.ai/api/mcp/auth_callback".to_string(),
                "https://chatgpt.com/connector/oauth/*".to_string(),
            ],
        }
    }

    fn app() -> Router {
        build_oauth(test_config())
    }

    async fn send(app: &Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 64).await.unwrap();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn get_auth_code(app: &Router) -> String {
        get_auth_code_with_challenge(app, None).await
    }

    async fn get_auth_code_with_challenge(app: &Router, challenge: Option<&str>) -> String {
        let mut uri = "/oauth/authorize?response_type=code&client_id=test-client&redirect_uri=https://claude.ai/api/mcp/auth_callback".to_string();
        if let Some(c) = challenge {
            uri.push_str(&format!("&code_challenge={}&code_challenge_method=S256", c));
        }

        let req = Request::builder().uri(&uri).body(Body::empty()).unwrap();

        let response = app.clone().oneshot(req).await.unwrap();
        let location = response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        let url = url::Url::parse(location).unwrap();
        url.query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .to_string()
    }

    fn token_request(body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    // -- well-known metadata --

    #[tokio::test]
    async fn oauth_metadata_returns_endpoints() {
        let app = app();
        let req = Request::builder()
            .uri("/.well-known/oauth-authorization-server")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();

        let (status, json) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["authorization_endpoint"],
            "http://example.com/oauth/authorize"
        );
        assert_eq!(json["token_endpoint"], "http://example.com/oauth/token");
    }

    #[tokio::test]
    async fn protected_resource_metadata_returns_resource() {
        let app = app();
        let req = Request::builder()
            .uri("/.well-known/oauth-protected-resource")
            .header("host", "example.com")
            .body(Body::empty())
            .unwrap();

        let (status, json) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["resource"], "http://example.com");
    }

    // -- authorize endpoint --

    #[tokio::test]
    async fn authorize_redirects_with_code_and_state() {
        let app = app();
        let req = Request::builder()
            .uri("/oauth/authorize?response_type=code&client_id=test-client&redirect_uri=https://claude.ai/api/mcp/auth_callback&state=xyz")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let location = response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(location.starts_with("https://claude.ai/api/mcp/auth_callback?code="));
        assert!(location.contains("&state=xyz"));
    }

    #[tokio::test]
    async fn authorize_rejects_wrong_client_id() {
        let (status, json) = send(
            &app(),
            Request::builder()
                .uri("/oauth/authorize?response_type=code&client_id=wrong&redirect_uri=https://claude.ai/api/mcp/auth_callback")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "invalid_client");
    }

    #[tokio::test]
    async fn authorize_rejects_disallowed_redirect_uri() {
        let (status, json) = send(
            &app(),
            Request::builder()
                .uri("/oauth/authorize?response_type=code&client_id=test-client&redirect_uri=https://evil.com/steal")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "invalid_request");
    }

    #[tokio::test]
    async fn authorize_allows_wildcard_redirect_uri() {
        let req = Request::builder()
            .uri("/oauth/authorize?response_type=code&client_id=test-client&redirect_uri=https://chatgpt.com/connector/oauth/abc123")
            .body(Body::empty())
            .unwrap();

        let response = app().clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    // -- token exchange --

    #[tokio::test]
    async fn token_exchange_success() {
        let app = app();
        let code = get_auth_code(&app).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret",
            code
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(json["access_token"].is_string());
        assert!(json["refresh_token"].is_string());
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["expires_in"], 365 * 24 * 60 * 60);
    }

    #[tokio::test]
    async fn token_exchange_wrong_secret() {
        let app = app();
        let code = get_auth_code(&app).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=wrong",
            code
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"], "invalid_client");
    }

    #[tokio::test]
    async fn token_exchange_invalid_code() {
        let (status, json) = send(
            &app(),
            token_request("grant_type=authorization_code&code=bogus&client_id=test-client&client_secret=test-secret"),
        ).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn token_exchange_unsupported_grant_type() {
        let (status, json) = send(
            &app(),
            token_request("grant_type=password&username=admin&password=admin"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "unsupported_grant_type");
    }

    // -- refresh token flow --

    #[tokio::test]
    async fn refresh_token_success() {
        let app = app();
        let code = get_auth_code(&app).await;

        // Exchange code for tokens
        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret",
            code
        );
        let (_, json) = send(&app, token_request(&body)).await;
        let refresh = json["refresh_token"].as_str().unwrap();

        // Refresh
        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_secret=test-secret",
            refresh
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(json["access_token"].is_string());
        assert!(json["refresh_token"].is_string());
    }

    #[tokio::test]
    async fn refresh_token_wrong_secret() {
        let app = app();
        let code = get_auth_code(&app).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret",
            code
        );
        let (_, json) = send(&app, token_request(&body)).await;
        let refresh = json["refresh_token"].as_str().unwrap();

        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_secret=wrong",
            refresh
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["error"], "invalid_client");
    }

    #[tokio::test]
    async fn refresh_token_invalid() {
        let (status, json) = send(
            &app(),
            token_request(
                "grant_type=refresh_token&refresh_token=garbage&client_secret=test-secret",
            ),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "invalid_grant");
    }

    // -- PKCE flow --

    #[tokio::test]
    async fn pkce_success() {
        let app = app();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));

        let code = get_auth_code_with_challenge(&app, Some(&challenge)).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret&code_verifier={}",
            code, verifier
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(json["access_token"].is_string());
    }

    #[tokio::test]
    async fn pkce_wrong_verifier() {
        let app = app();

        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(b"correct-verifier"));

        let code = get_auth_code_with_challenge(&app, Some(&challenge)).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret&code_verifier=wrong-verifier",
            code
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn pkce_missing_verifier() {
        let app = app();

        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(b"some-verifier"));

        let code = get_auth_code_with_challenge(&app, Some(&challenge)).await;

        let body = format!(
            "grant_type=authorization_code&code={}&client_id=test-client&client_secret=test-secret",
            code
        );
        let (status, json) = send(&app, token_request(&body)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            json["error_description"]
                .as_str()
                .unwrap()
                .contains("code_verifier required")
        );
    }
}
