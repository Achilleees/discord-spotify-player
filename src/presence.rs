#[derive(Debug, Clone)]
pub enum PresenceUpdate {
    Idle,
    Paused {
        title: String,
        artist: String,
        track_id: String,
    },
    Playing {
        title: String,
        artist: String,
        track_id: String,
        album_art_url: Option<String>,
    },
}
