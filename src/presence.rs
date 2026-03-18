#[derive(Clone, Debug)]
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
