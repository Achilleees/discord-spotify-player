use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

pub const REDIRECT_URI: &str = "http://127.0.0.1:8766/callback";

pub struct SpotifyOAuth {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    http: Client,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub token_type: String,
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
    UrlParse(url::ParseError),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OAuthError::Http(e) => write!(f, "HTTP error: {}", e),
            OAuthError::Api(msg) => write!(f, "API error: {}", msg),
            OAuthError::UrlParse(e) => write!(f, "URL parse error: {}", e),
        }
    }
}

impl std::error::Error for OAuthError {}

impl From<reqwest::Error> for OAuthError {
    fn from(e: reqwest::Error) -> Self {
        OAuthError::Http(e)
    }
}

impl From<url::ParseError> for OAuthError {
    fn from(e: url::ParseError) -> Self {
        OAuthError::UrlParse(e)
    }
}

impl SpotifyOAuth {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            redirect_uri: REDIRECT_URI.to_string(),
            http: Client::new(),
        }
    }

    /// Build the Spotify authorization URL (standard code flow).
    pub fn auth_url(&self, state: &str) -> String {
        let scopes = "streaming user-read-playback-state user-modify-playback-state user-read-currently-playing user-read-private";
        format!(
            "https://accounts.spotify.com/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&state={}",
            pct_encode(&self.client_id),
            pct_encode(scopes),
            pct_encode(&self.redirect_uri),
            pct_encode(state),
        )
    }

    /// Extract the `code` param from a redirect URL, or treat input as the code directly.
    pub fn extract_code(input: &str) -> Option<String> {
        let trimmed = input.trim();
        if let Ok(url) = Url::parse(trimmed) {
            if let Some((_, value)) = url.query_pairs().find(|(k, _)| k == "code") {
                return Some(value.into_owned());
            }
        }
        // Raw code: no spaces, looks like a token
        if !trimmed.is_empty() && !trimmed.contains(' ') && trimmed.len() > 20 {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    fn basic_auth_header(&self) -> String {
        let creds = format!("{}:{}", self.client_id, self.client_secret);
        format!("Basic {}", STANDARD.encode(creds.as_bytes()))
    }

    pub async fn exchange_code(&self, code: &str) -> Result<TokenResponse, OAuthError> {
        let mut params = HashMap::new();
        params.insert("grant_type", "authorization_code");
        params.insert("code", code);
        params.insert("redirect_uri", self.redirect_uri.as_str());

        let resp = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .header("Authorization", self.basic_auth_header())
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

        let resp = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .header("Authorization", self.basic_auth_header())
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
            .get("https://api.spotify.com/v1/me")
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

fn pct_encode(input: &str) -> String {
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
