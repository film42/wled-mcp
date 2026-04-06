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
use rmcp::transport::auth::AuthorizationMetadata;
use serde::Deserialize;
use sha2::Sha256;
use tracing::{debug, error, info, warn};

/// 365 days in seconds.
const TOKEN_EXPIRES_IN: u64 = 365 * 24 * 60 * 60;

/// Auth codes expire after 10 minutes.
const AUTH_CODE_EXPIRES_IN: u64 = 10 * 60;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub allowed_redirect_uris: Vec<String>,
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

/// Compute HMAC-SHA256 and return the raw bytes.
fn hmac_sign(secret: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key size");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// Constant-time comparison of two byte slices.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[derive(Clone, Debug)]
pub struct OAuthStore {
    config: OAuthConfig,
}

impl OAuthStore {
    pub fn new(config: OAuthConfig) -> Self {
        Self { config }
    }

    /// Check if a redirect URI matches the allowlist. Patterns ending in `*` are prefix matches.
    fn is_redirect_allowed(&self, uri: &str) -> bool {
        self.config.allowed_redirect_uris.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                uri.starts_with(prefix)
            } else {
                uri == pattern
            }
        })
    }

    /// Mint an HMAC-signed token. Format: `base64(payload).base64(signature)`
    /// The signature is HMAC-SHA256(client_secret, payload).
    /// No storage needed — validated by recomputing the HMAC.
    fn mint_token(&self, client_id: &str) -> (String, u64) {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = Self::build_signed_token(&self.config.client_secret, client_id, issued_at);
        (token, issued_at)
    }

    fn build_signed_token(secret: &str, client_id: &str, issued_at: u64) -> String {
        let payload = format!("{}:{}", client_id, issued_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let signature = hmac_sign(secret.as_bytes(), payload.as_bytes());
        format!("{}.{}", b64.encode(&payload), b64.encode(&signature))
    }

    /// Validate an HMAC-signed token. Returns true if signature matches and not expired.
    pub fn validate_token(&self, token: &str) -> bool {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let Some((payload_b64, sig_b64)) = token.split_once('.') else {
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

        // Verify HMAC (constant-time)
        let expected_sig = hmac_sign(self.config.client_secret.as_bytes(), payload.as_bytes());
        let Ok(actual_sig) = b64.decode(sig_b64) else {
            return false;
        };
        constant_time_eq(&expected_sig, &actual_sig)
    }

    /// Mint a token response JSON.
    fn token_response(&self, client_id: &str) -> serde_json::Value {
        let (access_token, issued_at) = self.mint_token(client_id);
        let refresh_payload = format!("refresh:{}:{}", client_id, issued_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let signature = hmac_sign(
            self.config.client_secret.as_bytes(),
            refresh_payload.as_bytes(),
        );
        let refresh_token = format!(
            "{}.{}",
            b64.encode(&refresh_payload),
            b64.encode(&signature)
        );

        serde_json::json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "expires_in": TOKEN_EXPIRES_IN,
            "refresh_token": refresh_token,
        })
    }

    /// Validate a refresh token. Returns true if signature is valid and not expired.
    fn validate_refresh_token(&self, token: &str) -> bool {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let Some((payload_b64, sig_b64)) = token.split_once('.') else {
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

        // Check expiry (365 days)
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now - issued_at > TOKEN_EXPIRES_IN {
            return false;
        }

        // Check client_id
        if client_id != self.config.client_id {
            return false;
        }

        // Verify HMAC (constant-time)
        let expected_sig = hmac_sign(self.config.client_secret.as_bytes(), payload.as_bytes());
        let Ok(actual_sig) = b64.decode(sig_b64) else {
            return false;
        };
        constant_time_eq(&expected_sig, &actual_sig)
    }

    /// Mint a stateless auth code. Format: `base64(payload).base64(signature)`
    /// Payload: `authcode:client_id:code_challenge:issued_at`
    /// (code_challenge is "none" if PKCE was not requested)
    fn mint_auth_code(&self, client_id: &str, code_challenge: Option<&str>) -> String {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let challenge = code_challenge.unwrap_or("none");
        let payload = format!("authcode:{}:{}:{}", client_id, challenge, issued_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let signature = hmac_sign(self.config.client_secret.as_bytes(), payload.as_bytes());
        format!("{}.{}", b64.encode(&payload), b64.encode(&signature))
    }

    /// Validate and decode a stateless auth code. Returns (client_id, code_challenge) on success.
    fn validate_auth_code(&self, code: &str) -> Option<(String, Option<String>)> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let (payload_b64, sig_b64) = code.split_once('.')?;

        let payload_bytes = b64.decode(payload_b64).ok()?;
        let payload = String::from_utf8(payload_bytes).ok()?;

        // Parse authcode:client_id:code_challenge:issued_at
        let rest = payload.strip_prefix("authcode:")?;
        let (rest, issued_at_str) = rest.rsplit_once(':')?;
        let issued_at = issued_at_str.parse::<u64>().ok()?;

        // Check expiry
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now - issued_at > AUTH_CODE_EXPIRES_IN {
            return None;
        }

        let (client_id, challenge) = rest.rsplit_once(':')?;

        // Check client_id
        if client_id != self.config.client_id {
            return None;
        }

        // Verify HMAC (constant-time)
        let expected_sig = hmac_sign(self.config.client_secret.as_bytes(), payload.as_bytes());
        let actual_sig = b64.decode(sig_b64).ok()?;
        if !constant_time_eq(&expected_sig, &actual_sig) {
            return None;
        }

        let code_challenge = if challenge == "none" {
            None
        } else {
            Some(challenge.to_string())
        };

        Some((client_id.to_string(), code_challenge))
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

/// GET /oauth/authorize — auto-approves and redirects with a stateless signed code
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

    if !store.is_redirect_allowed(&params.redirect_uri) {
        warn!("rejected redirect_uri: {}", params.redirect_uri);
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "redirect_uri not in allowlist"
            })),
        )
            .into_response();
    }

    let auth_code = store.mint_auth_code(&params.client_id, params.code_challenge.as_deref());

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

    // Validate client_secret (constant-time)
    if !constant_time_eq(
        req.client_secret.as_bytes(),
        store.config.client_secret.as_bytes(),
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "bad client_secret"
            })),
        )
            .into_response();
    }

    // Validate the stateless auth code
    let (code_client_id, code_challenge) = match store.validate_auth_code(&req.code) {
        Some(result) => result,
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
    if let Some(challenge) = &code_challenge {
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
    (StatusCode::OK, Json(store.token_response(&code_client_id))).into_response()
}

async fn handle_refresh_token(store: Arc<OAuthStore>, req: TokenRequest) -> Response {
    // Validate client_secret (constant-time)
    if !constant_time_eq(
        req.client_secret.as_bytes(),
        store.config.client_secret.as_bytes(),
    ) {
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

    metadata.scopes_supported = Some(vec!["mcp".to_string()]);
    metadata.response_types_supported = Some(vec!["code".to_string()]);
    metadata.code_challenge_methods_supported = Some(vec!["S256".to_string()]);

    (StatusCode::OK, Json(metadata))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OAuthConfig {
        OAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: "test-secret".to_string(),
            allowed_redirect_uris: vec![
                "https://chatgpt.com/connector/oauth/*".to_string(),
                "https://claude.ai/api/mcp/auth_callback".to_string(),
                "https://claude.com/api/mcp/auth_callback".to_string(),
            ],
        }
    }

    fn test_store() -> OAuthStore {
        OAuthStore::new(test_config())
    }

    // -- redirect URI allowlist --

    #[test]
    fn redirect_exact_match() {
        let store = test_store();
        assert!(store.is_redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(store.is_redirect_allowed("https://claude.com/api/mcp/auth_callback"));
    }

    #[test]
    fn redirect_wildcard_match() {
        let store = test_store();
        assert!(store.is_redirect_allowed("https://chatgpt.com/connector/oauth/abc123"));
        assert!(store.is_redirect_allowed("https://chatgpt.com/connector/oauth/anything/here"));
    }

    #[test]
    fn redirect_rejects_unknown() {
        let store = test_store();
        assert!(!store.is_redirect_allowed("https://evil.com/callback"));
        assert!(!store.is_redirect_allowed("https://claude.ai/api/mcp/other"));
        assert!(!store.is_redirect_allowed("http://claude.ai/api/mcp/auth_callback"));
    }

    #[test]
    fn redirect_empty_allowlist_rejects_all() {
        let store = OAuthStore::new(OAuthConfig {
            client_id: "test".to_string(),
            client_secret: "test".to_string(),
            allowed_redirect_uris: vec![],
        });
        assert!(!store.is_redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
    }

    // -- access token minting and validation --

    #[test]
    fn mint_and_validate_token() {
        let store = test_store();
        let (token, _) = store.mint_token("test-client");
        assert!(store.validate_token(&token));
    }

    #[test]
    fn token_wrong_secret_rejected() {
        let store = test_store();
        let (token, _) = store.mint_token("test-client");

        let other_store = OAuthStore::new(OAuthConfig {
            client_secret: "wrong-secret".to_string(),
            ..test_config()
        });
        assert!(!other_store.validate_token(&token));
    }

    #[test]
    fn token_wrong_client_id_rejected() {
        let store = test_store();
        let (token, _) = store.mint_token("test-client");

        let other_store = OAuthStore::new(OAuthConfig {
            client_id: "other-client".to_string(),
            ..test_config()
        });
        assert!(!other_store.validate_token(&token));
    }

    #[test]
    fn token_expired_rejected() {
        let store = test_store();
        let expired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - TOKEN_EXPIRES_IN
            - 1;
        let token = OAuthStore::build_signed_token("test-secret", "test-client", expired_at);
        assert!(!store.validate_token(&token));
    }

    #[test]
    fn token_garbage_rejected() {
        let store = test_store();
        assert!(!store.validate_token("garbage"));
        assert!(!store.validate_token("not.valid"));
        assert!(!store.validate_token(""));
    }

    #[test]
    fn token_tampered_signature_rejected() {
        let store = test_store();
        let (token, _) = store.mint_token("test-client");
        let tampered = format!("{}X", token);
        assert!(!store.validate_token(&tampered));
    }

    // -- refresh token minting and validation --

    #[test]
    fn refresh_token_roundtrip() {
        let store = test_store();
        let response = store.token_response("test-client");
        let refresh = response["refresh_token"].as_str().unwrap();
        assert!(store.validate_refresh_token(refresh));
    }

    #[test]
    fn refresh_token_wrong_secret_rejected() {
        let store = test_store();
        let response = store.token_response("test-client");
        let refresh = response["refresh_token"].as_str().unwrap();

        let other_store = OAuthStore::new(OAuthConfig {
            client_secret: "wrong-secret".to_string(),
            ..test_config()
        });
        assert!(!other_store.validate_refresh_token(refresh));
    }

    #[test]
    fn refresh_token_expired_rejected() {
        let store = test_store();
        let expired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - TOKEN_EXPIRES_IN
            - 1;

        let payload = format!("refresh:test-client:{}", expired_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let sig = hmac_sign("test-secret".as_bytes(), payload.as_bytes());
        let token = format!("{}.{}", b64.encode(&payload), b64.encode(&sig));

        assert!(!store.validate_refresh_token(&token));
    }

    #[test]
    fn access_token_not_valid_as_refresh() {
        let store = test_store();
        let (access, _) = store.mint_token("test-client");
        assert!(!store.validate_refresh_token(&access));
    }

    #[test]
    fn refresh_token_not_valid_as_access() {
        let store = test_store();
        let response = store.token_response("test-client");
        let refresh = response["refresh_token"].as_str().unwrap();
        assert!(!store.validate_token(refresh));
    }

    // -- auth code minting and validation --

    #[test]
    fn auth_code_roundtrip() {
        let store = test_store();
        let code = store.mint_auth_code("test-client", None);
        let result = store.validate_auth_code(&code);
        assert!(result.is_some());
        let (client_id, challenge) = result.unwrap();
        assert_eq!(client_id, "test-client");
        assert!(challenge.is_none());
    }

    #[test]
    fn auth_code_with_pkce_challenge() {
        let store = test_store();
        let code = store.mint_auth_code("test-client", Some("challenge-value"));
        let result = store.validate_auth_code(&code).unwrap();
        assert_eq!(result.0, "test-client");
        assert_eq!(result.1.as_deref(), Some("challenge-value"));
    }

    #[test]
    fn auth_code_expired_rejected() {
        let store = test_store();
        let expired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - AUTH_CODE_EXPIRES_IN
            - 1;

        let payload = format!("authcode:test-client:none:{}", expired_at);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let sig = hmac_sign("test-secret".as_bytes(), payload.as_bytes());
        let code = format!("{}.{}", b64.encode(&payload), b64.encode(&sig));

        assert!(store.validate_auth_code(&code).is_none());
    }

    #[test]
    fn auth_code_wrong_secret_rejected() {
        let store = test_store();
        let other_store = OAuthStore::new(OAuthConfig {
            client_secret: "other-secret".to_string(),
            ..test_config()
        });
        let code = other_store.mint_auth_code("test-client", None);
        assert!(store.validate_auth_code(&code).is_none());
    }

    // -- base_url_from_headers --

    #[test]
    fn base_url_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "example.com".parse().unwrap());
        assert_eq!(base_url_from_headers(&headers), "https://example.com");
    }

    #[test]
    fn base_url_host_header_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "myserver:3000".parse().unwrap());
        assert_eq!(base_url_from_headers(&headers), "http://myserver:3000");
    }

    #[test]
    fn base_url_no_headers() {
        let headers = HeaderMap::new();
        assert_eq!(base_url_from_headers(&headers), "http://localhost");
    }

    // -- constant_time_eq --

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(constant_time_eq(b"", b""));
    }
}
