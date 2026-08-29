//! OAuth 2.0 device authorization grant (RFC 8628) against Spotify's
//! desktop client ID. The user visits `verification_uri_complete` on any
//! device and approves; meanwhile this client polls `/api/token` until the
//! grant completes, is denied, or expires.

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;

/// Spotify's desktop client ID — the only one Spotify enables for the device
/// flow and for playback since 2026-08.
pub const DESKTOP_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

const SCOPES: &str = "streaming user-read-playback-state user-modify-playback-state user-read-currently-playing user-read-private";

pub struct SpotifyOAuth {
    pub client_id: String,
    http: Client,
    /// Base URL for accounts.spotify.com endpoints (device auth/token exchange/refresh).
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

/// Response from `POST /oauth2/device/authorize`: the codes and URL the user
/// needs to approve the grant, plus the polling cadence for `/api/token`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

impl DeviceAuthorization {
    /// The URL to show the user — the pre-filled `verification_uri_complete`
    /// when Spotify returns one, otherwise the bare `verification_uri`.
    pub fn url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizeErrorBody {
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorBody {
    error: Option<String>,
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
    /// The user declined the authorization request.
    Denied,
    /// The device code (or the polling deadline) expired before the user
    /// approved the grant.
    Expired,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Http(e) => write!(f, "HTTP error: {}", e),
            OAuthError::Api(msg) => write!(f, "API error: {}", msg),
            OAuthError::Denied => write!(f, "authorization was denied"),
            OAuthError::Expired => write!(f, "authorization request expired"),
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
    pub fn new() -> Self {
        Self {
            client_id: DESKTOP_CLIENT_ID.to_string(),
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
    fn with_base_urls(accounts_base: &str, api_base: &str) -> Self {
        let mut o = Self::new();
        o.accounts_base = accounts_base.trim_end_matches('/').to_string();
        o.api_base = api_base.trim_end_matches('/').to_string();
        o
    }

    /// Start a device authorization grant: request a device/user code pair
    /// and the URL the user must visit to approve it.
    pub async fn request_device_code(&self) -> Result<DeviceAuthorization, OAuthError> {
        let mut params = HashMap::new();
        params.insert("client_id", self.client_id.as_str());
        params.insert("scope", SCOPES);

        let resp = self
            .http
            .post(format!("{}/oauth2/device/authorize", self.accounts_base))
            .form(&params)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let err = serde_json::from_str::<DeviceAuthorizeErrorBody>(&body)
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_else(|| format!("device authorization failed ({}): {}", status, body));
            return Err(OAuthError::Api(err));
        }

        Ok(resp.json::<DeviceAuthorization>().await?)
    }

    /// Poll `/api/token` for the outcome of a device authorization grant,
    /// honoring the server's requested `interval` (and `slow_down` backoff)
    /// until the grant completes or `max_wait`/`expires_in` elapses,
    /// whichever is sooner. Cancellation (e.g. the user giving up) is the
    /// caller's responsibility.
    pub async fn poll_device_token(
        &self,
        auth: &DeviceAuthorization,
        max_wait: std::time::Duration,
    ) -> Result<TokenResponse, OAuthError> {
        let expires_in = std::time::Duration::from_secs(auth.expires_in);
        let deadline = std::time::Instant::now() + expires_in.min(max_wait);
        let mut interval = std::time::Duration::from_secs(auth.interval.max(1));

        loop {
            if std::time::Instant::now() >= deadline {
                return Err(OAuthError::Expired);
            }

            let mut params = HashMap::new();
            params.insert("client_id", self.client_id.as_str());
            params.insert(
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code",
            );
            params.insert("device_code", auth.device_code.as_str());

            let resp = self
                .http
                .post(format!("{}/api/token", self.accounts_base))
                .form(&params)
                .send()
                .await?;

            if resp.status().is_success() {
                return Ok(resp.json::<TokenResponse>().await?);
            }

            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let error = serde_json::from_str::<TokenErrorBody>(&body)
                .ok()
                .and_then(|b| b.error);

            match error.as_deref() {
                Some("authorization_pending") => {
                    tokio::time::sleep(interval).await;
                }
                Some("slow_down") => {
                    interval += std::time::Duration::from_secs(5);
                    tokio::time::sleep(interval).await;
                }
                Some("access_denied") => return Err(OAuthError::Denied),
                Some("expired_token") => return Err(OAuthError::Expired),
                _ => {
                    return Err(OAuthError::Api(format!(
                        "token poll failed ({}): {}",
                        status, body
                    )))
                }
            }
        }
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
    fn pct_encode_leaves_unreserved_and_escapes_the_rest() {
        assert_eq!(pct_encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(pct_encode("a b/c:d"), "a%20b%2Fc%3Ad");
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

    /// Bind a local port and answer every request on it with `status` +
    /// `body`, one connection at a time, for as long as the test needs. Used
    /// where a test drives more than one request (e.g. polling).
    fn mock_http_repeating(status: &'static str, body: &'static str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
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

    /// Bind a local port and answer requests with successive `(status, body)`
    /// pairs, holding the last pair for any request beyond the list.
    fn mock_http_sequence(responses: Vec<(&'static str, &'static str)>) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            let mut last = ("500 Internal Server Error", "{}");
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
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
                let (status, body) = responses.next().unwrap_or(last);
                last = (status, body);
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
    async fn refresh_without_rotated_token_parses_as_none() {
        // Spotify often omits refresh_token on refresh; the caller keeps the
        // old one, so this must parse as None rather than fail.
        let base = mock_http_repeating("200 OK", r#"{"access_token":"AT2","expires_in":3600}"#);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let tok = oauth.refresh_access_token("rt").await.unwrap();
        assert_eq!(tok.access_token, "AT2");
        assert!(tok.refresh_token.is_none());
    }

    #[tokio::test]
    async fn profile_falls_back_to_id_when_display_name_is_null() {
        let base = mock_http_repeating("200 OK", r#"{"id":"user-id-1","display_name":null}"#);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        assert_eq!(oauth.get_user_profile("at").await.unwrap(), "user-id-1");
    }

    #[tokio::test]
    async fn profile_non_2xx_is_api_error() {
        let base = mock_http_repeating("401 Unauthorized", r#"{"error":"expired"}"#);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        assert!(matches!(
            oauth.get_user_profile("at").await,
            Err(OAuthError::Api(_))
        ));
    }

    #[tokio::test]
    async fn device_authorize_parses_response() {
        let base = mock_http_repeating(
            "200 OK",
            r#"{"device_code":"DC","user_code":"ABCD-EFGH","verification_uri":"https://spotify.com/device","verification_uri_complete":"https://spotify.com/device?code=ABCD-EFGH","expires_in":600,"interval":0}"#,
        );
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let auth = oauth.request_device_code().await.unwrap();
        assert_eq!(auth.device_code, "DC");
        assert_eq!(auth.user_code, "ABCD-EFGH");
        assert_eq!(auth.url(), "https://spotify.com/device?code=ABCD-EFGH");
    }

    #[tokio::test]
    async fn poll_returns_token_after_pending() {
        let base = mock_http_sequence(vec![
            ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
            (
                "200 OK",
                r#"{"access_token":"AT","refresh_token":"RT","expires_in":3600}"#,
            ),
        ]);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let auth = DeviceAuthorization {
            device_code: "DC".to_string(),
            user_code: "UC".to_string(),
            verification_uri: "https://spotify.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 60,
            interval: 0,
        };
        let tok = oauth
            .poll_device_token(&auth, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(tok.access_token, "AT");
    }

    #[tokio::test]
    async fn poll_backs_off_on_slow_down() {
        let base = mock_http_sequence(vec![
            ("400 Bad Request", r#"{"error":"slow_down"}"#),
            (
                "200 OK",
                r#"{"access_token":"AT","refresh_token":"RT","expires_in":3600}"#,
            ),
        ]);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let auth = DeviceAuthorization {
            device_code: "DC".to_string(),
            user_code: "UC".to_string(),
            verification_uri: "https://spotify.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 60,
            interval: 0,
        };
        let tok = oauth
            .poll_device_token(&auth, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(tok.access_token, "AT");
    }

    #[tokio::test]
    async fn poll_access_denied_maps_to_denied() {
        let base = mock_http_repeating("400 Bad Request", r#"{"error":"access_denied"}"#);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let auth = DeviceAuthorization {
            device_code: "DC".to_string(),
            user_code: "UC".to_string(),
            verification_uri: "https://spotify.com/device".to_string(),
            verification_uri_complete: None,
            expires_in: 60,
            interval: 0,
        };
        assert!(matches!(
            oauth
                .poll_device_token(&auth, std::time::Duration::from_secs(10))
                .await,
            Err(OAuthError::Denied)
        ));
    }

    #[tokio::test]
    async fn poll_expired_maps_to_expired() {
        let base = mock_http_repeating("400 Bad Request", r#"{"error":"authorization_pending"}"#);
        let oauth = SpotifyOAuth::with_base_urls(&base, &base);
        let auth = DeviceAuthorization {
            device_code: "DC".to_string(),
            user_code: "UC".to_string(),
            verification_uri: "https://spotify.com/device".to_string(),
            verification_uri_complete: None,
            // Zero max_wait means the deadline has already passed on first
            // check — no request is made, so Expired comes back immediately.
            expires_in: 60,
            interval: 0,
        };
        assert!(matches!(
            oauth
                .poll_device_token(&auth, std::time::Duration::from_secs(0))
                .await,
            Err(OAuthError::Expired)
        ));
    }
}
