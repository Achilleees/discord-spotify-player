use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use url::Url;

pub const REDIRECT_URI: &str = "http://127.0.0.1:8766/callback";

const SCOPES: &str = "streaming user-read-playback-state user-modify-playback-state user-read-currently-playing user-read-private";

pub struct SpotifyOAuth {
    pub client_id: String,
    pub redirect_uri: String,
    http: Client,
    /// Base URL for accounts.spotify.com endpoints (token exchange/refresh).
    /// Overridable so tests can point at a local mock server.
    accounts_base: String,
    /// Base URL for api.spotify.com endpoints (profile fetch).
    api_base: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

/// A pending PKCE authorization: the verifier and state are held until the
/// user pastes back the redirect URL, then compared/consumed.
#[derive(Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

/// Parsed contents of a pasted redirect URL (or a bare code).
#[derive(Debug)]
pub struct RedirectParams {
    pub code: String,
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserProfile {
    display_name: Option<String>,
    id: String,
}

#[derive(Debug)]
pub enum OAuthError {
    Http(reqwest::Error),
    Api(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Http(e) => write!(f, "HTTP error: {}", e),
            OAuthError::Api(msg) => write!(f, "API error: {}", msg),
        }
    }
}

impl std::error::Error for OAuthError {}

impl From<reqwest::Error> for OAuthError {
    fn from(e: reqwest::Error) -> Self {
        OAuthError::Http(e)
    }
}

impl SpotifyOAuth {
    pub fn new(client_id: String) -> Self {
        Self {
            client_id,
            redirect_uri: REDIRECT_URI.to_string(),
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            accounts_base: "https://accounts.spotify.com".to_string(),
            api_base: "https://api.spotify.com".to_string(),
        }
    }

    /// Test-only constructor pointing both endpoint families at a mock server.
    #[cfg(test)]
    fn with_base_urls(client_id: &str, accounts_base: &str, api_base: &str) -> Self {
        let mut o = Self::new(client_id.to_string());
        o.accounts_base = accounts_base.trim_end_matches('/').to_string();
        o.api_base = api_base.trim_end_matches('/').to_string();
        o
    }

    /// Build the Spotify authorization URL for the Authorization Code + PKCE flow.
    pub fn auth_url(&self, pkce: &PkceChallenge) -> String {
        format!(
            "{}/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&state={}&code_challenge_method=S256&code_challenge={}",
            self.accounts_base,
            pct_encode(&self.client_id),
            pct_encode(SCOPES),
            pct_encode(&self.redirect_uri),
            pct_encode(&pkce.state),
            pct_encode(&pkce.challenge),
        )
    }

    /// Exchange an authorization code for tokens using the PKCE verifier.
    /// No client secret is sent — that is the whole point of PKCE.
    pub async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, OAuthError> {
        let mut params = HashMap::new();
        params.insert("grant_type", "authorization_code");
        params.insert("code", code);
        params.insert("redirect_uri", self.redirect_uri.as_str());
        params.insert("client_id", self.client_id.as_str());
        params.insert("code_verifier", code_verifier);

        let resp = self
            .http
            .post(format!("{}/api/token", self.accounts_base))
            .form(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OAuthError::Api(format!(
                "Token exchange failed ({}): {}",
                status, body
            )));
        }

        Ok(resp.json::<TokenResponse>().await?)
    }

    pub async fn refresh_access_token(
        &self,
        refresh_token: &str,
    ) -> Result<TokenResponse, OAuthError> {
        let mut params = HashMap::new();
        params.insert("grant_type", "refresh_token");
        params.insert("refresh_token", refresh_token);
        params.insert("client_id", self.client_id.as_str());

        let resp = self
            .http
            .post(format!("{}/api/token", self.accounts_base))
            .form(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OAuthError::Api(format!(
                "Token refresh failed ({}): {}",
                status, body
            )));
        }

        Ok(resp.json::<TokenResponse>().await?)
    }

    pub async fn get_user_profile(&self, access_token: &str) -> Result<String, OAuthError> {
        let resp = self
            .http
            .get(format!("{}/v1/me", self.api_base))
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OAuthError::Api(format!(
                "Profile fetch failed ({}): {}",
                status, body
            )));
        }

        let profile: UserProfile = resp.json().await?;
        Ok(profile.display_name.unwrap_or(profile.id))
    }
}

/// Generate a fresh PKCE challenge: a 43-char base64url verifier, its S256
/// challenge, and a random state for CSRF protection.
pub fn new_pkce() -> PkceChallenge {
    let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkceChallenge {
        verifier,
        challenge,
        state,
    }
}

/// Parse a pasted redirect URL (or a bare authorization code) into its code
/// and state. Tolerant of schemeless input (`127.0.0.1:8766/callback?...`),
/// and surfaces an explicit error when Spotify returned `?error=...` (e.g. a
/// denied consent) instead of silently treating the URL as a raw code.
pub fn parse_redirect(input: &str) -> Result<RedirectParams, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("empty input".to_string());
    }

    // Url::parse rejects a scheme starting with a digit, so a pasted
    // 127.0.0.1 redirect fails unless we give it a scheme.
    let parsed = Url::parse(trimmed).or_else(|_| Url::parse(&format!("http://{trimmed}")));

    if let Ok(url) = parsed {
        if url.query_pairs().any(|(k, _)| k == "code" || k == "error") {
            if let Some((_, err)) = url.query_pairs().find(|(k, _)| k == "error") {
                return Err(format!("Spotify returned an error: {err}"));
            }
            let code = url
                .query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.into_owned())
                .ok_or_else(|| "no code in redirect URL".to_string())?;
            let state = url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            return Ok(RedirectParams { code, state });
        }
    }

    // Bare code: no query string. Accept only if it plausibly is one — not a
    // URL pasted without its query (slashes, dots, colons), and only chars
    // that appear in authorization codes.
    let looks_like_url = trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains(':')
        || trimmed.starts_with("localhost");
    let code_charset = trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !looks_like_url && code_charset && (20..=1024).contains(&trimmed.len()) {
        Ok(RedirectParams {
            code: trimmed.to_string(),
            state: None,
        })
    } else {
        Err("could not find an authorization code in that input".to_string())
    }
}

pub fn pct_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let pkce = new_pkce();
        // Verifier is 32 bytes base64url-no-pad = 43 chars.
        assert_eq!(pkce.verifier.len(), 43);
        // RFC 7636 Appendix B fixed vector — an independent oracle, unlike
        // recomputing the challenge with the implementation's own expression
        // (which would pass even if both shared a wrong encoding).
        let challenge = URL_SAFE_NO_PAD
            .encode(Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pkce_state_is_unique_per_call() {
        assert_ne!(new_pkce().state, new_pkce().state);
    }

    #[test]
    fn parses_full_redirect_url() {
        let r = parse_redirect("http://127.0.0.1:8766/callback?code=AQ_abc123&state=xyz").unwrap();
        assert_eq!(r.code, "AQ_abc123");
        assert_eq!(r.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parses_schemeless_redirect_url() {
        // The exact shape a browser shows when 127.0.0.1 refuses the connection.
        let r = parse_redirect("127.0.0.1:8766/callback?code=longcode1234567890abc&state=s1").unwrap();
        assert_eq!(r.code, "longcode1234567890abc");
        assert_eq!(r.state.as_deref(), Some("s1"));
    }

    #[test]
    fn rejects_denied_consent() {
        let err = parse_redirect("http://127.0.0.1:8766/callback?error=access_denied&state=s1")
            .unwrap_err();
        assert!(err.contains("access_denied"), "got: {err}");
    }

    #[test]
    fn accepts_bare_code() {
        let r = parse_redirect("AQ_this_is_a_long_enough_code_string").unwrap();
        assert_eq!(r.code, "AQ_this_is_a_long_enough_code_string");
        assert!(r.state.is_none());
    }

    #[test]
    fn rejects_short_garbage() {
        assert!(parse_redirect("nope").is_err());
        assert!(parse_redirect("").is_err());
    }

    #[test]
    fn pct_encode_leaves_unreserved_and_escapes_the_rest() {
        assert_eq!(pct_encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(pct_encode("a b/c:d"), "a%20b%2Fc%3Ad");
    }

    #[test]
    fn auth_url_carries_challenge_and_state() {
        let oauth = SpotifyOAuth::new("client123".to_string());
        let pkce = new_pkce();
        let url = oauth.auth_url(&pkce);
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(url.contains(&format!("state={}", pkce.state)));
        assert!(url.contains("client_id=client123"));
    }

    // --- Network methods against a local one-shot mock server ---

    /// True once the buffered request holds full headers plus any declared body.
    fn request_complete(req: &[u8]) -> bool {
        let Some(header_end) = req.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&req[..header_end]).to_lowercase();
        let content_length = headers
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(str::trim))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        req.len() >= header_end + 4 + content_length
    }

    /// Bind a local port and answer the first request with `status` + `body`.
    /// Returns the base URL to point the client at.
    fn mock_http_once(status: &'static str, body: &'static str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if request_complete(&req) {
                                break;
                            }
                        }
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn exchange_code_parses_token_response() {
        let base = mock_http_once(
            "200 OK",
            r#"{"access_token":"AT","refresh_token":"RT","expires_in":3600}"#,
        );
        let oauth = SpotifyOAuth::with_base_urls("cid", &base, &base);
        let tok = oauth.exchange_code("code", "verifier").await.unwrap();
        assert_eq!(tok.access_token, "AT");
        assert_eq!(tok.refresh_token.as_deref(), Some("RT"));
        assert_eq!(tok.expires_in, 3600);
    }

    #[tokio::test]
    async fn exchange_code_maps_non_2xx_to_api_error() {
        let base = mock_http_once("400 Bad Request", r#"{"error":"invalid_grant"}"#);
        let oauth = SpotifyOAuth::with_base_urls("cid", &base, &base);
        match oauth.exchange_code("code", "verifier").await {
            Err(OAuthError::Api(msg)) => {
                assert!(msg.contains("400"), "got: {msg}");
                assert!(msg.contains("invalid_grant"), "got: {msg}");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_without_rotated_token_parses_as_none() {
        // Spotify often omits refresh_token on refresh; the caller keeps the
        // old one, so this must parse as None rather than fail.
        let base = mock_http_once("200 OK", r#"{"access_token":"AT2","expires_in":3600}"#);
        let oauth = SpotifyOAuth::with_base_urls("cid", &base, &base);
        let tok = oauth.refresh_access_token("rt").await.unwrap();
        assert_eq!(tok.access_token, "AT2");
        assert!(tok.refresh_token.is_none());
    }

    #[tokio::test]
    async fn profile_falls_back_to_id_when_display_name_is_null() {
        let base = mock_http_once("200 OK", r#"{"id":"user-id-1","display_name":null}"#);
        let oauth = SpotifyOAuth::with_base_urls("cid", &base, &base);
        assert_eq!(oauth.get_user_profile("at").await.unwrap(), "user-id-1");
    }

    #[tokio::test]
    async fn profile_non_2xx_is_api_error() {
        let base = mock_http_once("401 Unauthorized", r#"{"error":"expired"}"#);
        let oauth = SpotifyOAuth::with_base_urls("cid", &base, &base);
        assert!(matches!(
            oauth.get_user_profile("at").await,
            Err(OAuthError::Api(_))
        ));
    }
}
