//! Player core — the bot's own notion of playback intent.
//!
//! [`state`] is the pure decision core: one owned `PlayerState`, advanced by
//! `step(state, input, now) -> Vec<Effect>`. A single player actor owns the
//! state and its mailbox, and **the actor awaits nothing, ever**: every
//! effect is a synchronous channel send (`Spirc`, `TrackHandle`,
//! `ClearBridge`, `Presence`, `Ui`, `Reply`) or a spawn (`StartMedia`,
//! `JoinVoice`, `Announce`, `SetTimer`), so a `step` can never park the
//! mailbox behind IO. Session bring-up and Spotify metadata lookups run in
//! the interaction-handler task *before* an `Enqueue` is sent — never inside
//! the actor. Cross-event ordering lives in exactly two places: effect order
//! within one `step`, and runner-side gates on `Effect::StartMedia`. Timers
//! are spawned sleeps that come back as `Input::Tick`, and asynchronous
//! completions (media runners, voice joins, link changes) come back as
//! inputs tagged with an epoch or generation so stale reports are ignored.

pub mod state;
