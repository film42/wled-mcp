use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use hmac::{Hmac, Mac};
use rmcp::transport::auth::{AuthorizationMetadata, ClientRegistrationResponse};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// 365 days in seconds.
const TOKEN_EXPIRES_IN: u64 = 365 * 24 * 60 * 60;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

/// Derive the base URL from the incoming request headers.
/// Checks X-Forwarded-Proto/X-Forwarded-Host first (reverse proxy),
/// then falls back to the Host header.
pub fn base_url_from_headers(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| {
            if headers.get("x-forwarded-host").is_some() {
                "https"
            } else {
                "http"
            }
        });

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");

    format!("{}://{}", proto, host)
}

#[derive(Clone, Debug)]
pub struct OAuthStore {
    config: OAuthConfig,
    /// auth_code -> AuthSession (ephemeral, consumed within seconds)
    auth_sessions: Arc<RwLock<HashMap<String, AuthSession>>>,
}

#[derive(Clone, Debug)]
struct AuthSession {
    client_id: String,
    #[allow(dead_code)]
    redirect_uri: String,
    #[allow(dead_code)]
    state: Option<String>,
    code_challenge: Option<String>,
}

impl OAuthStore {
    pub fn new(config: OAuthConfig) -> Self {
        Self {
            config,
            auth_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Mint an HMAC-signed token. Format: `base64(client_id:issued_at).base64(signature)`
    /// The signature is HMAC-SHA256(client_secret, "client_id:issued_at").
    /// No storage needed — validated by recomputing the HMAC.
    fn mint_token(&self, client_id: &str) -> (String, u64) {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = Self::sign_token(&self.config.client_secret, client_id, issued_at);
        (token, issued_at)
    }

    fn sign_token(secret: &str, client_id: &str, issued_at: u64) -> String {
        let payload = format!("{}:{}", client_id, issued_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(payload.as_bytes());
        let signature = mac.finalize().into_bytes();

        format!("{}.{}", b64.encode(&payload), b64.encode(&signature))
    }

    /// Validate an HMAC-signed token. Returns true if signature matches and not expired.
    pub fn validate_token(&self, token: &str) -> bool {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let Some((payload_b64, _sig_b64)) = token.split_once('.') else {
            return false;
        };

        let Ok(payload_bytes) = b64.decode(payload_b64) else {
            return false;
        };
        let Ok(payload) = String::from_utf8(payload_bytes) else {
            return false;
        };

        // Parse client_id:issued_at
        let Some((client_id, issued_at_str)) = payload.rsplit_once(':') else {
            return false;
        };
        let Ok(issued_at) = issued_at_str.parse::<u64>() else {
            return false;
        };

        // Check expiry
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now - issued_at > TOKEN_EXPIRES_IN {
            return false;
        }

        // Check client_id matches
        if client_id != self.config.client_id {
            return false;
        }

        // Verify HMAC
        let expected = Self::sign_token(&self.config.client_secret, client_id, issued_at);
        token == expected
    }

    /// Mint a token response JSON.
    fn token_response(&self, client_id: &str) -> serde_json::Value {
        let (access_token, issued_at) = self.mint_token(client_id);
        // Refresh token is just another signed token with a "refresh:" prefix in the payload
        let refresh_payload = format!("refresh:{}:{}", client_id, issued_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut mac = HmacSha256::new_from_slice(self.config.client_secret.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(refresh_payload.as_bytes());
        let sig = mac.finalize().into_bytes();
        let refresh_token = format!("{}.{}", b64.encode(&refresh_payload), b64.encode(&sig));

        serde_json::json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": TOKEN_EXPIRES_IN,
            "refresh_token": refresh_token,
        })
    }

    /// Validate a refresh token. Returns true if signature is valid (no expiry check on refresh).
    fn validate_refresh_token(&self, token: &str) -> bool {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let Some((payload_b64, _sig_b64)) = token.split_once('.') else {
            return false;
        };

        let Ok(payload_bytes) = b64.decode(payload_b64) else {
            return false;
        };
        let Ok(payload) = String::from_utf8(payload_bytes) else {
            return false;
        };

        // Must start with "refresh:"
        let Some(rest) = payload.strip_prefix("refresh:") else {
            return false;
        };

        // Parse client_id:issued_at
        let Some((client_id, issued_at_str)) = rest.rsplit_once(':') else {
            return false;
        };
        let Ok(issued_at) = issued_at_str.parse::<u64>() else {
            return false;
        };

        // Check client_id
        if client_id != self.config.client_id {
            return false;
        }

        // Verify HMAC
        let refresh_payload = format!("refresh:{}:{}", client_id, issued_at);
        let mut mac = HmacSha256::new_from_slice(self.config.client_secret.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(refresh_payload.as_bytes());
        let sig = mac.finalize().into_bytes();
        let expected = format!("{}.{}", b64.encode(&refresh_payload), b64.encode(&sig));

        token == expected
    }
}

// --- OAuth Endpoints ---

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    #[allow(dead_code)]
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    #[allow(dead_code)]
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    #[allow(dead_code)]
    pub code_challenge_method: Option<String>,
}

/// GET /oauth/authorize — auto-approves and redirects with code
pub async fn oauth_authorize(
    Query(params): Query<AuthorizeQuery>,
    State(store): State<Arc<OAuthStore>>,
) -> Response {
    debug!("oauth_authorize: client_id={}", params.client_id);

    if params.client_id != store.config.client_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "unknown client_id"
            })),
        )
            .into_response();
    }

    let auth_code = format!("code-{}", Uuid::new_v4());

    let session = AuthSession {
        client_id: params.client_id,
        redirect_uri: params.redirect_uri.clone(),
        state: params.state.clone(),
        code_challenge: params.code_challenge,
    };

    store
        .auth_sessions
        .write()
        .await
        .insert(auth_code.clone(), session);

    let mut redirect_url = format!("{}?code={}", params.redirect_uri, auth_code);
    if let Some(state) = &params.state {
        redirect_url.push_str(&format!("&state={}", state));
    }

    info!("auto-approved authorization, redirecting");
    Redirect::to(&redirect_url).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub redirect_uri: String,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub refresh_token: String,
}

/// POST /oauth/token — exchange code or refresh token for access token
pub async fn oauth_token(State(store): State<Arc<OAuthStore>>, request: Request<Body>) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(e) => {
            error!("failed to read token request body: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_request"})),
            )
                .into_response();
        }
    };

    let req: TokenRequest = match serde_urlencoded::from_bytes(&bytes) {
        Ok(r) => r,
        Err(e) => {
            error!("failed to parse token request: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_request",
                    "error_description": format!("bad form data: {}", e)
                })),
            )
                .into_response();
        }
    };

    match req.grant_type.as_str() {
        "authorization_code" => handle_authorization_code(store, req).await,
        "refresh_token" => handle_refresh_token(store, req).await,
        other => {
            warn!("unsupported grant_type: {}", other);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "unsupported_grant_type",
                    "error_description": format!("unsupported: {}", other)
                })),
            )
                .into_response()
        }
    }
}

async fn handle_authorization_code(store: Arc<OAuthStore>, req: TokenRequest) -> Response {
    let client_id = if req.client_id.is_empty() {
        store.config.client_id.clone()
    } else {
        req.client_id.clone()
    };

    if client_id != store.config.client_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_client"})),
        )
            .into_response();
    }

    // Validate client_secret
    if req.client_secret != store.config.client_secret {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "bad client_secret"
            })),
        )
            .into_response();
    }

    // Look up and consume the auth code
    let session = store.auth_sessions.write().await.remove(&req.code);
    let session = match session {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_grant",
                    "error_description": "invalid or expired authorization code"
                })),
            )
                .into_response();
        }
    };

    // Validate PKCE if a code_challenge was provided
    if let Some(challenge) = &session.code_challenge {
        match &req.code_verifier {
            Some(verifier) => {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(verifier.as_bytes());
                let computed =
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
                if computed != *challenge {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "invalid_grant",
                            "error_description": "PKCE verification failed"
                        })),
                    )
                        .into_response();
                }
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "code_verifier required"
                    })),
                )
                    .into_response();
            }
        }
    }

    info!("minted 365-day access token for client_id={}", client_id);
    (
        StatusCode::OK,
        Json(store.token_response(&session.client_id)),
    )
        .into_response()
}

async fn handle_refresh_token(store: Arc<OAuthStore>, req: TokenRequest) -> Response {
    // Validate client_secret
    if req.client_secret != store.config.client_secret {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "bad client_secret"
            })),
        )
            .into_response();
    }

    if !store.validate_refresh_token(&req.refresh_token) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "invalid refresh token"
            })),
        )
            .into_response();
    }

    info!("refreshed token for client_id={}", store.config.client_id);
    (
        StatusCode::OK,
        Json(store.token_response(&store.config.client_id)),
    )
        .into_response()
}

/// GET /.well-known/oauth-authorization-server
pub async fn oauth_metadata(headers: HeaderMap) -> impl IntoResponse {
    let base = base_url_from_headers(&headers);
    let mut metadata = AuthorizationMetadata::default();
    metadata.issuer = Some(base.clone());
    metadata.authorization_endpoint = format!("{}/oauth/authorize", base);
    metadata.token_endpoint = format!("{}/oauth/token", base);
    metadata.registration_endpoint = Some(format!("{}/oauth/register", base));
    metadata.scopes_supported = Some(vec!["mcp".to_string()]);
    metadata.response_types_supported = Some(vec!["code".to_string()]);
    metadata.code_challenge_methods_supported = Some(vec!["S256".to_string()]);

    (StatusCode::OK, Json(metadata))
}

/// Dynamic client registration — returns the pre-configured client credentials.
/// Claude needs this endpoint to exist.
#[derive(Debug, Deserialize)]
pub struct RegistrationRequest {
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

pub async fn oauth_register(
    State(store): State<Arc<OAuthStore>>,
    Json(req): Json<RegistrationRequest>,
) -> Response {
    debug!("oauth_register: {:?}", req);

    if req.redirect_uris.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "at least one redirect_uri required"
            })),
        )
            .into_response();
    }

    // Return the pre-configured client credentials so Claude can use them
    let mut response =
        ClientRegistrationResponse::new(store.config.client_id.clone(), req.redirect_uris);
    response.client_secret = Some(store.config.client_secret.clone());
    response.client_name = req.client_name;

    info!(
        "registered client with pre-configured client_id={}",
        store.config.client_id
    );
    (StatusCode::CREATED, Json(response)).into_response()
}

/// Bearer token validation middleware for the /mcp route
pub async fn auth_middleware(
    State(store): State<Arc<OAuthStore>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let auth_header = request.headers().get("Authorization");

    let token = match auth_header {
        Some(header) => {
            let header_str = header.to_str().unwrap_or("");
            match header_str.strip_prefix("Bearer ") {
                Some(t) => t.to_string(),
                None => {
                    return (StatusCode::UNAUTHORIZED, [("WWW-Authenticate", "Bearer")])
                        .into_response();
                }
            }
        }
        None => {
            let base = base_url_from_headers(request.headers());
            let www_auth = format!(
                "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                base
            );
            return (StatusCode::UNAUTHORIZED, [("WWW-Authenticate", www_auth)]).into_response();
        }
    };

    if store.validate_token(&token) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [("WWW-Authenticate", "Bearer error=\"invalid_token\"")],
        )
            .into_response()
    }
}

/// GET /.well-known/oauth-protected-resource
pub async fn protected_resource_metadata(headers: HeaderMap) -> impl IntoResponse {
    let base = base_url_from_headers(&headers);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "resource": base,
            "authorization_servers": [base],
            "scopes_supported": ["mcp"],
            "bearer_methods_supported": ["header"],
        })),
    )
}
