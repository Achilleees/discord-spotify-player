//! Shared HTTP client for Spotify Web API calls.

use std::sync::OnceLock;
use std::time::Duration;

/// One client for every Web API call site (playback commands, queue add,
/// track metadata): connection reuse plus a hard timeout, so a hung Spotify
/// endpoint can't stall a caller — the presence loop awaits metadata fetches
/// inline, where an untimed hang would freeze play/pause mirroring.
pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client construction only fails on invalid builder config")
    })
}
