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
}

#[derive(Debug, Deserialize, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[allow(dead_code)]
    pub token_type: String,
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
        }
    }

    /// Build the Spotify authorization URL for the Authorization Code + PKCE flow.
    pub fn auth_url(&self, pkce: &PkceChallenge) -> String {
        format!(
            "https://accounts.spotify.com/authorize?response_type=code&client_id={}&scope={}&redirect_uri={}&state={}&code_challenge_method=S256&code_challenge={}",
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
            .post("https://accounts.spotify.com/api/token")
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
            .post("https://accounts.spotify.com/api/token")
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

    // Bare code: no query string. Accept only if it plausibly is one.
    if !trimmed.contains(' ') && (20..=1024).contains(&trimmed.len()) {
        Ok(RedirectParams {
            code: trimmed.to_string(),
            state: None,
        })
    } else {
        Err("could not find an authorization code in that input".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = new_pkce();
        // Verifier is 32 bytes base64url-no-pad = 43 chars.
        assert_eq!(pkce.verifier.len(), 43);
        // Challenge recomputes deterministically from the verifier.
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
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
}
