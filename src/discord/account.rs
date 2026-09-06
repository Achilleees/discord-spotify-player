//! Account lifecycle: `/login`, `/logout`, `/forget`, boot auto-start, and
//! the account-switch bookkeeping shared by every path that changes whose
//! Spotify session is live.
//!
//! `Handler`'s struct definition and its `EventHandler` impl live in
//! `bot.rs`; the methods here are an `impl Handler` split into this module
//! for file size — see `discord::commands` for the slash-command dispatch
//! that calls into `handle_login`/`handle_logout`/`handle_forget`, and
//! `bot::ready`/`bot::voice_state_update` for the two callers of
//! `auto_start_stored_session`/`teardown_playback_session`.

use super::bot::Handler;
use super::ui::UiMsg;
use crate::oauth::DeviceAuthorization;
use crate::player::state::{Input as PlayerInput, VoiceGuard};
use crate::spotify::EnsureOutcome;
use crate::users::UserCredentials;
use serenity::all::UserId;
use serenity::builder::CreateMessage;
use serenity::client::Context;
use std::sync::Arc;
use tokio::sync::Notify;

/// How long to poll Spotify for a device-code pairing before giving up.
const DEVICE_LOGIN_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Display-only cache of who the live Spotify session belongs to, for `/who`
/// and the takeover gate. The `SessionSupervisor` owns the actual session
/// lifecycle (the librespot task, its refresher, the generation) — this is
/// just the name and id `finish_account_switch` (the shared tail of every
/// account-switch path) / `supervisor.stop` populate and clear alongside it.
pub struct ActiveSession {
    pub discord_user_id: u64,
    pub discord_name: String,
}

/// Outcome of `/login`: either a plain reply, or a freshly issued device-code
/// pairing that the caller must show the user and then poll to completion.
pub(super) enum LoginOutcome {
    Reply(String),
    Pair(DeviceAuthorization),
}

impl Handler {
    /// The Discord user id of the current session owner, if any.
    fn active_owner(&self) -> Option<u64> {
        let lock = self.active_session.lock();
        lock.as_ref().map(|s| s.discord_user_id)
    }

    /// Full playback teardown: silence the player (media cancelled, queue
    /// cleared), abort any Spotify session (deactivating its owner), reset
    /// the controls card, and optionally leave voice. Runs when the voice
    /// channel empties and when the bot is force-disconnected.
    pub(super) async fn teardown_playback_session(&self, ctx: &Context, leave_voice: bool, expected_voice: u64) {
        // VoiceLost first (mailbox order beats the runner's own cancel
        // report): the actor drops any active media turn and stale-ifies
        // the runner's coming `MediaEnded`. The awaited Stop then releases
        // the turn before the supervisor's `LinkDown` lands, so nothing
        // gets promoted into the emptying call, and the actor's own
        // presence/status transitions cover the Idle update. The queue is
        // deliberately left alone: it survives an empty channel the same way
        // it survives a restart, and `/clear` is the only thing that empties
        // it. Voice is handled below, never by the actor: its `LeaveVoice`
        // would arm the deliberate-leave guard, and after a force disconnect
        // no gateway echo ever comes to consume it.
        let Some(retirement) = self.voice_owner.retire_music_if(expected_voice) else { return; };
        self.player.send(PlayerInput::VoiceLost);
        let _ = self.player.stop_without_leaving().await;

        let owner = {
            let mut lock = self.active_session.lock();
            lock.take().map(|session| session.discord_user_id)
        };
        if let Some(owner) = owner {
            self.supervisor.stop(owner).await;
            tracing::info!(user = owner, "aborted session (teardown)");
        }

        let tx = { self.ui_tx.lock().clone() };
        if let Some(tx) = tx {
            let _ = tx.send(UiMsg::Idle { account: None });
        }

        if leave_voice {
            let _transition = self.voice_owner.transitions.lock().await;
            if let Some(manager) = songbird::get(ctx)
                .await
                .filter(|_| self.voice_owner.retirement_current(retirement))
            {
                // `remove`, not `leave`: leave keeps the Call registered and
                // every later presence check would read it as "still in a
                // call".
                self.leaving_voice
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if manager.remove(self.guild_id).await.is_err() {
                    super::bot::consume_deliberate_leave(&self.leaving_voice);
                }
                tracing::info!("bot left voice channel");
            }
        }

        // Deactivate only the session owner, not every stored user.
        if let Some(owner) = owner {
            let _ = self.user_store.deactivate(&owner.to_string());
        }
    }

    /// Ensure the bot is in a voice call, following `discord_user_id` in
    /// when it has to join fresh — a no-op when a call already exists, so a
    /// session switch never replays the join sound or re-hooks the bridge
    /// reader over a call that's already up (which would cut whatever media
    /// item is currently feeding it).
    async fn ensure_voice_for_user(
        &self,
        discord_user_id: Option<u64>,
        guard: Option<VoiceGuard>,
    ) -> Option<u64> {
        (self.join_voice)(discord_user_id, guard).await
    }

    /// Restart the stored active user's Spotify session on boot, through the
    /// exact same path /login uses. Skips when no user is marked active or the
    /// stored record is unusable (unparseable id, failed refresh).
    ///
    /// A thin wrapper over `SessionSupervisor::ensure_session`, the same
    /// on-demand bring-up `/play` and `/queue` use, so the refresh-and-switch
    /// work is single-sourced. What is boot-specific stays here: the
    /// pre-attempt log line (kept ahead of the call so it reads as
    /// "attempting", not "succeeded"), and the two outcomes `ensure_session`
    /// deliberately does not decide for its other callers — deactivate-and-
    /// warn on a dead refresh token, and the account-switch bookkeeping
    /// (voice, UI, DB exclusivity, the `/who` cache) on success. That goes
    /// through `finish_account_switch` rather than `switch_active_session`,
    /// so `ensure_session`'s own `supervisor.switch` is not immediately
    /// followed by a second one.
    pub(super) async fn auto_start_stored_session(&self) {
        let Some(user) = self.user_store.list().into_iter().find(|u| u.active) else {
            tracing::info!("auto-start skipped: no stored active user");
            return;
        };
        let Ok(discord_user_id) = user.discord_user_id.parse::<u64>() else {
            tracing::warn!(user = %user.discord_user_id, "auto-start skipped: unparseable discord user id");
            return;
        };

        tracing::info!(spotify = %user.discord_name, "auto-starting stored session");
        println!("Auto-starting Spotify session for {}...", user.discord_name);

        match self.supervisor.ensure_session(&self.oauth, &self.user_store).await {
            EnsureOutcome::Ready(_gen) => {
                self.finish_account_switch(discord_user_id, user.discord_name, None)
                    .await;
            }
            EnsureOutcome::NoAccount => {
                // Unreachable in practice: `user` above already found this
                // exact row active, and nothing else can flip it inactive
                // this early in boot. Logged rather than assumed.
                tracing::warn!("auto-start: ensure_session found no active account after one was found above");
            }
            EnsureOutcome::Failed(reason) => {
                tracing::warn!(error = %reason, "auto-start token refresh failed; skipping auto-start");
                // Dead stored token: deactivate it so every boot stops
                // retrying, and say so in the text channel — a silent
                // skip looks like the bot lost Spotify support entirely.
                let _ = self.user_store.deactivate(&user.discord_user_id);
                let ctx = {
                    let lock = self.ctx.lock();
                    lock.clone()
                };
                if let Some(ctx) = ctx {
                    let msg = CreateMessage::new().content(format!(
                        "⚠️ Couldn't restore **{}**'s Spotify session (stored credentials expired). Run `/login` to reconnect.",
                        user.discord_name
                    ));
                    let _ = self.text_channel_id.send_message(&ctx, msg).await;
                }
            }
        }
    }

    /// Point the live Spotify session at `discord_user_id` and update
    /// everything downstream of an account change: the DB's exclusive-active
    /// flag, the `/who`/takeover-gate display cache, the voice call (a no-op
    /// when one already exists), and the card's account name. Never touches
    /// the player — a media item already playing keeps playing straight
    /// through a login, and the actor drops the replaced session's armed
    /// track itself when the new session's `LinkUp` reaches it.
    async fn switch_active_session(
        &self,
        discord_user_id: u64,
        discord_name: String,
        access_token: String,
        refresh_token: String,
        expires_in: u64,
        voice_guard: Option<VoiceGuard>,
    ) {
        self.supervisor
            .switch(discord_user_id, discord_name.clone(), access_token, refresh_token, expires_in)
            .await;
        let revision = self
            .finish_account_switch(discord_user_id, discord_name, voice_guard.clone())
            .await;
        let guard = match voice_guard {
            Some(mut guard) => {
                let Some(revision) = revision else {
                    return;
                };
                guard.generation = revision;
                guard.may_join = false;
                Some(guard)
            }
            None => None,
        };
        // `/login` is a human claim on the device: activate it, so the
        // bot shows as the playing device right away. Only this explicit
        // path does — boot auto-start and on-demand sessions never
        // activate (F15). Sent only once the new session's `LinkUp` has
        // landed: `LinkUp` resets the device to inactive, so an activation
        // racing ahead of it was silently undone.
        let player = self.player.clone();
        let mut link_up = self.supervisor.link_up_watch();
        tokio::spawn(async move {
            let wait = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while link_up.borrow_and_update().is_none() {
                    if link_up.changed().await.is_err() {
                        return false;
                    }
                }
                true
            });
            if matches!(wait.await, Ok(true)) {
                // The mailbox is FIFO and the session sends `LinkUp` before
                // it flips the watch, so this lands after it.
                player.send(match guard {
                    Some(guard) => PlayerInput::Guarded {
                        guard,
                        input: Box::new(PlayerInput::ActivateDevice),
                    },
                    None => PlayerInput::ActivateDevice,
                });
            }
        });
    }

    /// The half of an account switch that isn't the supervisor's job (see
    /// `spotify::session`'s import restriction, which keeps it from ever
    /// reaching voice, the DB's exclusivity flag or the UI): DB exclusivity,
    /// the `/who`/takeover-gate cache, the voice call (a no-op when one
    /// already exists), and the card's account name. Split out of
    /// `switch_active_session` so `auto_start_stored_session` can run it
    /// after `ensure_session`'s own `supervisor.switch` without a second,
    /// redundant one.
    async fn finish_account_switch(
        &self,
        discord_user_id: u64,
        discord_name: String,
        guard: Option<VoiceGuard>,
    ) -> Option<u64> {
        // Exactly one user stays active:true, so auto-start can't resurrect a
        // displaced user after a restart.
        if let Err(e) = self.user_store.set_active_exclusive(&discord_user_id.to_string()) {
            tracing::warn!(error = %e, "failed to set exclusive active user");
        }

        {
            let mut lock = self.active_session.lock();
            *lock = Some(ActiveSession { discord_user_id, discord_name: discord_name.clone() });
        }

        let revision = self
            .ensure_voice_for_user(Some(discord_user_id), guard)
            .await;

        let tx = { self.ui_tx.lock().clone() };
        if let Some(tx) = tx {
            let _ = tx.send(UiMsg::AccountChanged(Some(discord_name)));
        }
        revision
    }

    pub(super) async fn handle_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        in_voice: bool,
        voice_guard: Option<crate::player::state::VoiceGuard>,
    ) -> LoginOutcome {
        // Taking over an active session owned by someone else requires being in
        // the bot's voice channel — you can't evict the current DJ from outside.
        if let Some(owner) = self.active_owner() {
            if owner != user_id_u64 && !in_voice {
                return LoginOutcome::Reply("Someone else is the active DJ. Join the bot's voice channel to take over.".to_string());
            }
        }

        // Stored creds exist: quick re-login by refreshing, no new pairing needed.
        if let Some(existing) = self.user_store.load(user_id) {
            return LoginOutcome::Reply(
                self.reactivate_login(
                    user_id,
                    user_id_u64,
                    discord_username,
                    existing,
                    voice_guard,
                )
                .await,
            );
        }

        // Fresh login: start a device-code pairing.
        match self.oauth.request_device_code().await {
            Ok(auth) => LoginOutcome::Pair(auth),
            Err(e) => LoginOutcome::Reply(format!("Couldn't start a Spotify login: {e}. Try again.")),
        }
    }

    /// Quick re-login for a user who already authorized once: refresh their
    /// token and (re)start the session without a new browser round-trip.
    async fn reactivate_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        existing: UserCredentials,
        voice_guard: Option<crate::player::state::VoiceGuard>,
    ) -> String {
        match self.oauth.refresh_access_token(&existing.refresh_token).await {
            Ok(new_token) => {
                let expires_in = new_token.expires_in;
                let mut creds = existing.clone();
                let activate = voice_guard
                    .as_ref()
                    .is_none_or(|guard| self.voice_guard_current(guard));
                creds.active = activate || existing.active;
                creds.access_token = new_token.access_token.clone();
                if let Some(rt) = new_token.refresh_token {
                    creds.refresh_token = rt;
                }
                if let Err(e) = self.user_store.save(&creds) {
                    tracing::error!(error = %e, "failed to save reactivated session");
                    return "Failed to save session. Please try again.".to_string();
                }
                if !activate {
                    return "Saved your login. Join voice and open a fresh music panel to activate it.".into();
                }
                self.switch_active_session(
                    user_id_u64,
                    discord_username.to_string(),
                    new_token.access_token,
                    creds.refresh_token.clone(),
                    expires_in,
                    voice_guard,
                )
                .await;
                tracing::info!(user = %user_id, name = %discord_username, "session reactivated");
                format!(
                    "Session (re)started for **{}**! Pick **{}** in Spotify's device list to play.",
                    discord_username, self.config.device_name
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "token refresh failed on reactivation; re-authorization required");
                // The stored refresh token is dead. Deactivate it so
                // auto-start stops retrying it, and prompt a fresh
                // authorization instead of dead-ending the user into a
                // /forget + /login round-trip.
                let _ = self.user_store.deactivate(user_id);
                format!(
                    "Your stored Spotify session for **{}** can't be refreshed — run `/login` again to re-authorize.",
                    existing.discord_name
                )
            }
        }
    }

    /// Persist device-flow tokens for this user with the given active flag,
    /// using the Discord display name as the shown Spotify name (the Web API
    /// profile lookup is gone — it 429s under the desktop client ID). Returns
    /// `(display_name, refresh_token)`, or the reply to send when the tokens
    /// can't be stored.
    async fn save_device_creds(
        &self,
        user_id: &str,
        discord_username: &str,
        token: &crate::oauth::TokenResponse,
        active: bool,
    ) -> Result<(String, String), String> {
        let Some(refresh_token) = token.refresh_token.clone() else {
            return Err("Spotify didn't return a refresh token. Run `/login` again.".to_string());
        };
        let display_name = discord_username.to_string();
        let creds = UserCredentials {
            discord_user_id: user_id.to_string(),
            discord_name: discord_username.to_string(),
            spotify_username: display_name.clone(),
            access_token: token.access_token.clone(),
            refresh_token: refresh_token.clone(),
            active,
        };
        if let Err(e) = self.user_store.save(&creds) {
            tracing::error!(error = %e, "failed to save credentials");
            return Err("Failed to save credentials. Please try again.".to_string());
        }
        Ok((display_name, refresh_token))
    }

    /// Poll Spotify for the device-code pairing issued by `handle_login`,
    /// cancellably: a newer `/login` or a `/logout`/`/forget` for this user
    /// notifies the stashed `Notify`, which aborts this poll in place of the
    /// old one.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_device_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        discord_username: &str,
        ctx: &Context,
        auth: DeviceAuthorization,
        voice_guard: Option<crate::player::state::VoiceGuard>,
    ) -> String {
        let cancel = Arc::new(Notify::new());
        {
            let mut pending = self.pending_auth.lock();
            if let Some(old) = pending.insert(user_id_u64, cancel.clone()) {
                // A newer /login replaces (and cancels) any prior pairing poll.
                old.notify_one();
            }
        }

        let outcome = tokio::select! {
            r = self.oauth.poll_device_token(&auth, DEVICE_LOGIN_MAX_WAIT) => Some(r),
            _ = cancel.notified() => None,
        };

        {
            let mut pending = self.pending_auth.lock();
            // Only clear our own entry — a newer login may have already
            // replaced it with its own pending pairing.
            if let Some(current) = pending.get(&user_id_u64) {
                if Arc::ptr_eq(current, &cancel) {
                    pending.remove(&user_id_u64);
                }
            }
        }

        let token = match outcome {
            None => return "This login was cancelled by a newer `/login` or a logout.".to_string(),
            Some(Ok(t)) => t,
            Some(Err(crate::oauth::OAuthError::Denied)) => {
                return "Spotify login was declined.".to_string();
            }
            Some(Err(crate::oauth::OAuthError::Expired)) => {
                return "That code expired. Run `/login` again.".to_string();
            }
            Some(Err(e)) => {
                return format!("Spotify login failed: {e}. Run `/login` again.");
            }
        };

        // Taking over an active session owned by someone else requires being in
        // the bot's voice channel — you can't evict the current DJ from
        // outside. Re-checked here since the poll can take minutes. The
        // tokens are stored inactive so the retry is a quick re-login and the
        // current DJ's row stays the only active one.
        if voice_guard
            .as_ref()
            .is_some_and(|guard| !self.voice_guard_current(guard))
        {
            return match self.save_device_creds(user_id, discord_username, &token, false).await {
                Ok(_) => "Saved your login. Your voice room or music session changed; open a fresh music panel to activate it.".into(),
                Err(message) => message,
            };
        }
        if let Some(owner) = self.active_owner() {
            if owner != user_id_u64 && !self.user_in_bot_voice_channel(ctx, UserId::new(user_id_u64)) {
                return match self.save_device_creds(user_id, discord_username, &token, false).await {
                    Ok(_) => "Saved your Spotify login. Join the bot's voice channel and run `/login` again to take over.".to_string(),
                    Err(msg) => msg,
                };
            }
        }

        let (display_name, refresh_token) =
            match self.save_device_creds(user_id, discord_username, &token, true).await {
                Ok(v) => v,
                Err(msg) => return msg,
            };
        tracing::info!(user = %user_id, name = %display_name, "device login successful");
        self.switch_active_session(
            user_id_u64,
            display_name.clone(),
            token.access_token,
            refresh_token,
            token.expires_in,
            voice_guard,
        )
        .await;
        format!(
            "Logged in as **{display_name}**! Spotify session started.\n\
             Open Spotify on any device, tap the Connect (devices) icon, and pick \
             **{}** — it appears from anywhere, no shared network needed.",
            self.config.device_name
        )
    }

    pub(super) async fn handle_logout(&self, user_id: &str, user_id_u64: u64) -> String {
        // A pending device-code pairing for this user is now moot — cancel its poll.
        if let Some(cancel) = self.pending_auth.lock().remove(&user_id_u64) {
            cancel.notify_one();
        }

        // Only the owner of the live session may tear it down. A bystander's
        // /logout must not pause the DJ's audio or wipe the controls. The
        // supervisor re-checks ownership itself (`stop` is a no-op for a
        // non-owner); this flag is only for which reply text to show below.
        let owned_live_session = self.active_owner() == Some(user_id_u64);

        if owned_live_session {
            self.supervisor.stop(user_id_u64).await;
            {
                let mut lock = self.active_session.lock();
                *lock = None;
            }
            tracing::info!(user = %user_id, "active librespot session aborted");
            // Does NOT touch playback directly — the supervisor's `stop`
            // emits `LinkDown`, and the actor (the sole owner of the queue,
            // the armed track and the status line) decides what a dead link
            // means; a queued media item keeps playing straight through a
            // logout. The card is the player's, not the account's: only
            // the name on it changes.
            let tx = { self.ui_tx.lock().clone() };
            if let Some(tx) = tx {
                let _ = tx.send(UiMsg::AccountChanged(None));
            }
        }

        match self.user_store.deactivate(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "session deactivated"); "Session deactivated. Your credentials are kept — run `/login` to reactivate without re-authorizing.".to_string() }
            Ok(false) if owned_live_session => "Session stopped.".to_string(),
            Ok(false) => "You don't have an active session.".to_string(),
            Err(e) => { tracing::error!("failed to deactivate session: {}", e); "Failed to deactivate session.".to_string() }
        }
    }

    pub(super) async fn handle_forget(&self, user_id: &str, user_id_u64: u64) -> String {
        // A pending device-code pairing for this user is now moot — cancel its poll.
        if let Some(cancel) = self.pending_auth.lock().remove(&user_id_u64) {
            cancel.notify_one();
        }

        // Forgetting the account behind the live session must also end that
        // session — otherwise it keeps running on deleted credentials until
        // the process restarts, and nothing would auto-start it again. Only
        // the owner's own /forget does this; a bystander's touches nothing
        // but their own row. Playback is untouched either way: the supervisor
        // emits LinkDown and the actor decides what a dead link means, so a
        // queued media item plays straight through.
        if self.active_owner() == Some(user_id_u64) {
            self.supervisor.stop(user_id_u64).await;
            {
                let mut lock = self.active_session.lock();
                *lock = None;
            }
            let tx = { self.ui_tx.lock().clone() };
            if let Some(tx) = tx {
                let _ = tx.send(UiMsg::AccountChanged(None));
            }
            tracing::info!(user = %user_id, "live session ended by /forget");
        }

        match self.user_store.remove(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "credentials forgotten"); "✅ Credentials permanently deleted — run `/login` to connect again.".to_string() }
            Ok(false) => "No stored credentials to delete.".to_string(),
            Err(e) => { tracing::error!("failed to delete credentials: {}", e); "⚠️ Couldn't delete the credentials — try again.".to_string() }
        }
    }
}
