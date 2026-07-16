#[derive(Clone)]
pub enum PresenceUpdate {
    Idle,
    Paused,
    Playing {
        title: String,
        artist: String,
        track_id: String,
        access_token: String,
    },
}

// Manual Debug: the Playing variant carries a live OAuth access token, which
// a derived impl would write into logs on any `{:?}` (same treatment as
// UserCredentials).
impl std::fmt::Debug for PresenceUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceUpdate::Idle => write!(f, "Idle"),
            PresenceUpdate::Paused => write!(f, "Paused"),
            PresenceUpdate::Playing { title, artist, track_id, .. } => f
                .debug_struct("Playing")
                .field("title", title)
                .field("artist", artist)
                .field("track_id", track_id)
                .field("access_token", &"<redacted>")
                .finish(),
        }
    }
}
