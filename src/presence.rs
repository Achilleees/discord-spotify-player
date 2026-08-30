//! Status vocabulary for the bot's Discord presence line.
//!
//! Produced by the player actor (from `Effect::Presence` and its own media
//! starts) and consumed by `discord::presence::run_presence_loop`, which
//! renders it as the bot's activity text. Carries exactly what that line
//! needs: `Playing` names the track, `Paused`/`Idle` are bare states.

#[derive(Debug, Clone)]
pub enum PresenceUpdate {
    Idle,
    Paused,
    Playing { title: String, artist: String },
}
