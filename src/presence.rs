#[derive(Debug, Clone)]
/// `track_id` on `Paused` is read by the transport channel from C3.
#[allow(dead_code)]
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
