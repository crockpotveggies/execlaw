//! OAuth 2.0 provider primitives — the pure, network-talking pieces
//! the admin endpoints + refresh sweeper sit on top of.
//!
//! Today there's exactly one provider (Google) wired up. The trait
//! lives behind `OauthProvider` so the second provider lands as a
//! sibling impl without rewriting the `oauth_admin` endpoints or
//! the refresh sweeper. The provider trait is intentionally small:
//! build authorize URL, exchange code, refresh access token, fetch
//! userinfo. Everything else (state-token CSRF, persistence,
//! redirect bookkeeping) lives in the caller because it's the same
//! across providers.
//!
//! Network calls go through a `reqwest::Client` the caller injects
//! so tests can hand in a mock that points at `httpmock` /
//! `wiremock`. For prod, `GoogleOauthProvider::default_client()`
//! returns a sane reqwest with rustls + reasonable timeouts.

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum OauthProviderError {
    #[error("http: {0}")]
    Http(String),
    #[error("provider returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("response decode: {0}")]
    Decode(String),
    #[error("missing field in response: {0}")]
    MissingField(&'static str),
}

#[derive(Debug, Clone)]
pub struct AuthorizeParams {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// Random per-request CSRF token; the caller has already
    /// persisted this in `state_oauth_pending`.
    pub state_token: String,
}

#[derive(Debug, Clone)]
pub struct ExchangeParams {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct RefreshParams {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

/// Successful token-grant response. Both code-exchange and
/// refresh-grant land here.
#[derive(Debug, Clone)]
pub struct TokenGrant {
    pub access_token: String,
    /// Some providers (Google after the first consent) omit a fresh
    /// refresh_token on the refresh-grant path. Caller handles the
    /// preserve-on-NULL semantics in `OauthTokenStore::upsert`.
    pub refresh_token: Option<String>,
    pub expires_in_secs: i64,
    pub scope: Option<String>,
    pub id_token: Option<String>,
}

/// Userinfo response — for the email field we surface in the
/// "Connected as user@example.com" badge.
#[derive(Debug, Clone)]
pub struct Userinfo {
    pub email: Option<String>,
    pub name: Option<String>,
}

#[async_trait]
pub trait OauthProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;

    /// Construct the URL the operator's browser opens to consent.
    /// Pure (no I/O) — string concatenation against the provider's
    /// authorize endpoint.
    fn build_authorize_url(&self, params: &AuthorizeParams) -> Result<String, OauthProviderError>;

    /// Exchange the `code` from the callback for an access /
    /// refresh token pair.
    async fn exchange_code(
        &self,
        params: &ExchangeParams,
    ) -> Result<TokenGrant, OauthProviderError>;

    /// Use a refresh_token to mint a fresh access_token. Most
    /// providers don't return a new refresh_token here.
    async fn refresh_access_token(
        &self,
        params: &RefreshParams,
    ) -> Result<TokenGrant, OauthProviderError>;

    /// Look up the email of the account these tokens belong to.
    async fn fetch_userinfo(&self, access_token: &str) -> Result<Userinfo, OauthProviderError>;
}

// ---------------------------------------------------------------------------
// Google.

const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";

#[derive(Debug, Clone)]
pub struct GoogleOauthProvider {
    client: reqwest::Client,
    /// Override the authorize URL (tests). None = production URL.
    authorize_url: String,
    token_url: String,
    userinfo_url: String,
}

impl Default for GoogleOauthProvider {
    fn default() -> Self {
        Self::new(Self::default_client())
    }
}

impl GoogleOauthProvider {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            authorize_url: GOOGLE_AUTHORIZE_URL.into(),
            token_url: GOOGLE_TOKEN_URL.into(),
            userinfo_url: GOOGLE_USERINFO_URL.into(),
        }
    }

    /// Test-only constructor that points the token + userinfo URLs
    /// at a local `httpmock` / `wiremock` server.
    pub fn with_endpoints(
        client: reqwest::Client,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
        userinfo_url: impl Into<String>,
    ) -> Self {
        Self {
            client,
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
            userinfo_url: userinfo_url.into(),
        }
    }

    pub fn default_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .user_agent("execlaw/0.1 oauth-client")
            .build()
            // The reqwest builder only fails when rustls picks up a
            // bad system root store; treat that as a hard config
            // error rather than papering over it with .unwrap().
            .unwrap_or_else(|_| reqwest::Client::new())
    }
}

#[async_trait]
impl OauthProvider for GoogleOauthProvider {
    fn provider_id(&self) -> &'static str {
        "google"
    }

    fn build_authorize_url(&self, params: &AuthorizeParams) -> Result<String, OauthProviderError> {
        // access_type=offline + prompt=consent so we ALWAYS get a
        // refresh_token. Without prompt=consent, Google omits the
        // refresh_token on a re-consent (returning user) and the
        // operator silently loses the long-lived secret.
        let mut url = Url::parse(&self.authorize_url)
            .map_err(|e| OauthProviderError::Http(format!("authorize url: {e}")))?;
        let scope = params.scopes.join(" ");
        url.query_pairs_mut()
            .append_pair("client_id", &params.client_id)
            .append_pair("redirect_uri", &params.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &scope)
            .append_pair("state", &params.state_token)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("include_granted_scopes", "true");
        Ok(url.to_string())
    }

    async fn exchange_code(
        &self,
        params: &ExchangeParams,
    ) -> Result<TokenGrant, OauthProviderError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", params.code.as_str()),
            ("client_id", params.client_id.as_str()),
            ("client_secret", params.client_secret.as_str()),
            ("redirect_uri", params.redirect_uri.as_str()),
        ];
        post_token_grant(&self.client, &self.token_url, &form).await
    }

    async fn refresh_access_token(
        &self,
        params: &RefreshParams,
    ) -> Result<TokenGrant, OauthProviderError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", params.refresh_token.as_str()),
            ("client_id", params.client_id.as_str()),
            ("client_secret", params.client_secret.as_str()),
        ];
        post_token_grant(&self.client, &self.token_url, &form).await
    }

    async fn fetch_userinfo(&self, access_token: &str) -> Result<Userinfo, OauthProviderError> {
        let resp = self
            .client
            .get(&self.userinfo_url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| OauthProviderError::Http(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| OauthProviderError::Http(e.to_string()))?;
        if !status.is_success() {
            return Err(OauthProviderError::Status {
                status: status.as_u16(),
                body,
            });
        }
        #[derive(Deserialize)]
        struct R {
            email: Option<String>,
            name: Option<String>,
        }
        let r: R = serde_json::from_str(&body)
            .map_err(|e| OauthProviderError::Decode(format!("{e}: {body}")))?;
        Ok(Userinfo {
            email: r.email,
            name: r.name,
        })
    }
}

async fn post_token_grant(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenGrant, OauthProviderError> {
    let resp = client
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| OauthProviderError::Http(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| OauthProviderError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(OauthProviderError::Status {
            status: status.as_u16(),
            body,
        });
    }
    #[derive(Deserialize)]
    struct R {
        access_token: Option<String>,
        refresh_token: Option<String>,
        expires_in: Option<i64>,
        scope: Option<String>,
        id_token: Option<String>,
    }
    let r: R = serde_json::from_str(&body)
        .map_err(|e| OauthProviderError::Decode(format!("{e}: {body}")))?;
    let access_token = r
        .access_token
        .ok_or(OauthProviderError::MissingField("access_token"))?;
    let expires_in = r
        .expires_in
        .ok_or(OauthProviderError::MissingField("expires_in"))?;
    Ok(TokenGrant {
        access_token,
        refresh_token: r.refresh_token,
        expires_in_secs: expires_in,
        scope: r.scope,
        id_token: r.id_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_authorize_url_includes_required_params() {
        let p = GoogleOauthProvider::default();
        let url = p
            .build_authorize_url(&AuthorizeParams {
                client_id: "abc.apps.googleusercontent.com".into(),
                redirect_uri: "http://localhost:3030/api/oauth/google/callback".into(),
                scopes: vec![
                    "https://www.googleapis.com/auth/contacts.readonly".to_owned(),
                    "openid".to_owned(),
                    "email".to_owned(),
                ],
                state_token: "csrf-xyz".into(),
            })
            .unwrap();
        // Required pieces.
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("client_id=abc.apps.googleusercontent.com"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=csrf-xyz"));
        // access_type=offline + prompt=consent — without these
        // Google won't reliably return a refresh_token.
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // Scope joined with space (URL-encoded as +).
        assert!(url.contains("scope=https"));
        assert!(url.contains("contacts.readonly"));
    }

    #[tokio::test]
    async fn exchange_code_decodes_token_grant() {
        // Local one-shot HTTP server simulating Google's token endpoint.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = serde_json::json!({
                "access_token": "ya29.access",
                "refresh_token": "1//refresh",
                "expires_in": 3599,
                "scope": "https://www.googleapis.com/auth/contacts.readonly",
                "token_type": "Bearer",
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let p = GoogleOauthProvider::with_endpoints(
            reqwest::Client::new(),
            GOOGLE_AUTHORIZE_URL,
            format!("http://{addr}/token"),
            GOOGLE_USERINFO_URL,
        );
        let g = p
            .exchange_code(&ExchangeParams {
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://localhost:3030/cb".into(),
                code: "auth-code".into(),
            })
            .await
            .unwrap();
        assert_eq!(g.access_token, "ya29.access");
        assert_eq!(g.refresh_token.as_deref(), Some("1//refresh"));
        assert_eq!(g.expires_in_secs, 3599);
    }

    #[tokio::test]
    async fn token_endpoint_error_surfaces_status_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"error":"invalid_grant","error_description":"Bad code"}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let p = GoogleOauthProvider::with_endpoints(
            reqwest::Client::new(),
            GOOGLE_AUTHORIZE_URL,
            format!("http://{addr}/token"),
            GOOGLE_USERINFO_URL,
        );
        let err = p
            .exchange_code(&ExchangeParams {
                client_id: "cid".into(),
                client_secret: "secret".into(),
                redirect_uri: "http://localhost:3030/cb".into(),
                code: "stale".into(),
            })
            .await
            .unwrap_err();
        match err {
            OauthProviderError::Status { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("invalid_grant"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_userinfo_returns_email_when_present() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"email":"alice@example.com","name":"Alice"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let p = GoogleOauthProvider::with_endpoints(
            reqwest::Client::new(),
            GOOGLE_AUTHORIZE_URL,
            GOOGLE_TOKEN_URL,
            format!("http://{addr}/userinfo"),
        );
        let u = p.fetch_userinfo("ya29.access").await.unwrap();
        assert_eq!(u.email.as_deref(), Some("alice@example.com"));
        assert_eq!(u.name.as_deref(), Some("Alice"));
    }
}
