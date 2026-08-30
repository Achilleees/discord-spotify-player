//! Session supervisor — the Spotify session's own lifecycle, kept apart from
//! playback.
//!
//! [`SessionSupervisor`] owns everything a live Spotify Connect session
//! needs to exist: the librespot task, its proactive token refresher, the
//! shared token state and a monotonic session generation. This is lifecycle
//! (B) in the design — background, started by `/login`, boot auto-start, or
//! (later) on demand; stopped by the owner's `/logout`, an account switch,
//! or restart-budget exhaustion. It has no notion of "is anything audible" —
//! that is lifecycle (A), the player, which lives for the whole process and
//! is untouched by anything here.
//!
//! **This module imports no songbird, queue or player-effect type.** That
//! restriction is the point, not a style preference: the compiler — not
//! code review — guarantees a session change (`/login` mid-track) cannot
//! reach the queue, the feeder or a `TrackHandle`. The only surface it has
//! into the player core is [`crate::player::state::Input`], a plain enum of
//! *decisions to make*, delivered through a [`PlayerHandle`] mailbox exactly
//! like every other input source. What the core decides to do about a link
//! change — including whether anything reaches Spotify at all — is entirely
//! its own call.
//!
//! Every generation of session gets its own `LinkUp`/`LinkReconnecting`/
//! `LinkDown` bracket, tagged with a `u64` generation the player core
//! compares against its own `link_gen`, so a stale generation's straggling
//! report can never be read as current. `link_up_watch()` mirrors the same
//! generation as a `watch::Receiver<Option<u64>>` — written by each
//! generation's own task, *without* taking this supervisor's `switch`/`stop`
//! lock, specifically so `ensure_session` (on-demand session bring-up, C7)
//! can wait on it while holding no lock of its own; the device-auth pairing
//! poll that `switch` is called after can run for minutes, and nothing here
//! may block behind that.
//!
//! An account switch (`switch`) is deliberate, not a transient blip: the
//! previous session's Spotify-side device queue and any armed track die
//! with it, so switching never emits `LinkDown` for the session it is
//! replacing — that would invite the reconnect path's snapshot-and-restore
//! machinery to `Transfer` state from one account onto another. Only a
//! session's own natural end (restart budget exhausted) or an explicit
//! `stop` emits `LinkDown`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;

use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::SpotifyOAuth;
use crate::player::actor::PlayerHandle;
use crate::player::state::{Input, TransportEvent};
use crate::users::UserStore;

use super::player::{SpircCommand, SpotifyPlayer};

/// Refresh the access token this many seconds before it expires.
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
/// Floor on the proactive-refresh wait, so a short-lived token can't spin.
const TOKEN_REFRESH_MIN_WAIT_SECS: u64 = 30;
/// Backoff after a failed proactive refresh before retrying.
const TOKEN_REFRESH_RETRY_SECS: u64 = 30;
/// Give up the librespot reconnect loop after this many consecutive returns
/// without a healthy session, so a permanently-down Spotify can't hot-loop.
const MAX_SESSION_RESTARTS: u32 = 10;
/// Fallback token lifetime when the real `expires_in` is unknown.
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 3600;
/// A session that runs at least this long resets the restart budget — only
/// consecutive *fast* failures count toward giving up.
const MIN_STABLE_SESSION_SECS: u64 = 60;
/// Fixed backoff between session-restart attempts (each retried with a
/// freshly refreshed token).
const SESSION_RESTART_DELAY_SECS: u64 = 2;

/// One live session's task handles, aborted together. Distinct from
/// `discord::bot::ActiveSession`, which is a display-only cache the caller
/// populates *after* `switch` returns (for `/who` and the takeover gate) —
/// this is the supervisor's own bookkeeping and never leaves this module.
struct LiveSession {
    discord_user_id: u64,
    generation: u64,
    handle: JoinHandle<()>,
    refresh_handle: JoinHandle<()>,
}

impl LiveSession {
    fn abort(&self) {
        self.handle.abort();
        self.refresh_handle.abort();
    }
}

/// Owns the Spotify session lifecycle end to end. See the module docs for
/// the import restriction and the generation/watch contract.
pub struct SessionSupervisor {
    config: Arc<Config>,
    bridge: Arc<AudioBridge>,
    oauth: Arc<SpotifyOAuth>,
    user_store: Arc<UserStore>,
    /// Long-lived, shared by every generation's librespot task (unlike the
    /// old per-login channel, this is created once at startup). The
    /// receiver lives in `discord::bot`, feeding the presence-deriving
    /// transport shim, which reads `link_up_watch()` for the generation to
    /// stamp onto what it forwards to the player.
    transport_tx: mpsc::UnboundedSender<TransportEvent>,
    /// Where `LinkUp`/`LinkDown`/`LinkReconnecting` go: the player actor's
    /// mailbox.
    link_tx: PlayerHandle,
    /// C4-transitional dual-publish target: the exact same `Arc` the actor's
    /// `PlayerDeps.spirc_cmd_tx` holds and `discord::bot::Handler` reads
    /// (`has_spotify_session`, `lookup_spotify_track`) — one cell, so a
    /// single write here reaches both readers. C5 removes the second name.
    spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    generation: AtomicU64,
    /// Guards only `switch`/`stop` — never anything long-running; the tasks
    /// they spawn outlive the lock entirely. Behind its own `Arc` (rather
    /// than folded into `&self`) so a session's own give-up path can clear
    /// its slot without the supervisor itself needing to be `Arc`-wrapped.
    live: Arc<tokio::sync::Mutex<Option<LiveSession>>>,
    /// Carries the current generation while a session is up, `None` while
    /// down. Written by each generation's own task — see the module docs.
    link_up_tx: watch::Sender<Option<u64>>,
}

/// Outcome of [`SessionSupervisor::ensure_session`].
pub enum EnsureOutcome {
    /// A live session is up, at this generation — either it already was,
    /// or `ensure_session` just brought it up.
    Ready(u64),
    /// No stored user is marked active; there is nothing to start.
    NoAccount,
    /// A stored account exists but no live session could be brought up —
    /// the token refresh failed, or the link never came up within the
    /// wait. The `String` is a diagnostic for `tracing::warn!` only; per
    /// the reply-string convention (`player::state`'s `reply`), it must
    /// never reach a Discord reply directly.
    Failed(String),
}

impl SessionSupervisor {
    /// `spirc_cmd_tx` is the one addition beyond the plumbing every other
    /// dependency here is named for: it is how `switch`/`stop` publish (and
    /// retire) the live session's command sender for the player actor and
    /// `discord::bot::Handler` to use — see the field doc.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        bridge: Arc<AudioBridge>,
        oauth: Arc<SpotifyOAuth>,
        user_store: Arc<UserStore>,
        transport_tx: mpsc::UnboundedSender<TransportEvent>,
        link_tx: PlayerHandle,
        spirc_cmd_tx: Arc<Mutex<Option<mpsc::UnboundedSender<SpircCommand>>>>,
    ) -> Self {
        let (link_up_tx, _rx) = watch::channel::<Option<u64>>(None);
        Self {
            config,
            bridge,
            oauth,
            user_store,
            transport_tx,
            link_tx,
            spirc_cmd_tx,
            generation: AtomicU64::new(0),
            live: Arc::new(tokio::sync::Mutex::new(None)),
            link_up_tx,
        }
    }

    /// The generation currently up, `None` while down. Reading this takes no
    /// lock on the supervisor — see the module docs.
    pub fn link_up_watch(&self) -> watch::Receiver<Option<u64>> {
        self.link_up_tx.subscribe()
    }

    /// (Re)point the live Spotify session at `discord_user_id`: aborts the
    /// previous session and its refresher, bumps the generation, and spawns
    /// the new librespot task and its proactive token refresher. Every other
    /// side effect `spawn_session` used to bundle in here — voice, the DB's
    /// active flag, the UI card — is the caller's job now (see
    /// `discord::bot`'s callers); this function touches none of them.
    pub async fn switch(
        &self,
        discord_user_id: u64,
        discord_name: String,
        access_token: String,
        refresh_token: String,
        expires_in: u64,
    ) {
        let mut live = self.live.lock().await;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);

        if let Some(old) = live.take() {
            tracing::info!(old_user = old.discord_user_id, "aborting existing librespot session");
            let tx = self.spirc_cmd_tx.lock().take();
            if let Some(tx) = tx {
                if tx.send(SpircCommand::Shutdown).is_ok() {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            }
            old.abort();
            // No LinkDown for the generation being replaced — see the
            // module docs: a switch is a deliberate account change, not a
            // blip, and its Spotify-side queue (and any armed track) is
            // gone with it, not a candidate for the reconnect path's
            // snapshot-and-restore.
        }

        let user_id_str = discord_user_id.to_string();

        // Shared, single-owner token state. The refresher below is the only
        // writer of the refresh token; the librespot task only reads the
        // current access token and signals the refresher when its session
        // dies.
        let token_state = Arc::new(Mutex::new((access_token, refresh_token)));
        let refresh_now = Arc::new(Notify::new());

        let (spirc_tx, spirc_rx) = mpsc::unbounded_channel::<SpircCommand>();
        *self.spirc_cmd_tx.lock() = Some(spirc_tx);

        // Proactive refresher: the sole owner of the refresh cycle. Wakes on
        // a timer (expires_in − margin) or when the librespot task signals
        // its session died, refreshes, and publishes the new access token to
        // the shared state and the DB.
        let refresh_handle = tokio::spawn({
            let oauth = self.oauth.clone();
            let user_store = self.user_store.clone();
            let token_state = token_state.clone();
            let refresh_now = refresh_now.clone();
            let user_id_str = user_id_str.clone();
            async move {
                let mut lifetime =
                    if expires_in == 0 { DEFAULT_TOKEN_LIFETIME_SECS } else { expires_in };
                loop {
                    let wait = lifetime
                        .saturating_sub(TOKEN_REFRESH_MARGIN_SECS)
                        .max(TOKEN_REFRESH_MIN_WAIT_SECS);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                        _ = refresh_now.notified() => {
                            tracing::debug!(user = discord_user_id, "early token refresh requested");
                        }
                    }
                    let current_refresh = { token_state.lock().1.clone() };
                    match oauth.refresh_access_token(&current_refresh).await {
                        Ok(tok) => {
                            let new_refresh = tok.refresh_token.clone().unwrap_or(current_refresh);
                            {
                                let mut s = token_state.lock();
                                s.0 = tok.access_token.clone();
                                s.1 = new_refresh.clone();
                            }
                            if let Some(mut creds) = user_store.load(&user_id_str) {
                                creds.access_token = tok.access_token.clone();
                                creds.refresh_token = new_refresh;
                                let _ = user_store.save(&creds);
                            }
                            lifetime =
                                if tok.expires_in == 0 { DEFAULT_TOKEN_LIFETIME_SECS } else { tok.expires_in };
                            tracing::info!(user = discord_user_id, lifetime, "access token refreshed");
                        }
                        Err(e) => {
                            tracing::warn!(user = discord_user_id, error = ?e, "token refresh failed; retrying");
                            // Wait out the retry window on the next loop.
                            lifetime = TOKEN_REFRESH_RETRY_SECS + TOKEN_REFRESH_MARGIN_SECS;
                        }
                    }
                }
            }
        });

        let config = self.config.clone();
        let bridge = self.bridge.clone();
        let transport_tx = self.transport_tx.clone();
        let link_tx = self.link_tx.clone();
        let link_up_tx = self.link_up_tx.clone();
        let live_slot = self.live.clone();
        let spirc_cmd_tx_slot = self.spirc_cmd_tx.clone();

        let handle = tokio::spawn(async move {
            // Written directly here, not through the supervisor's `live`
            // lock — see the module docs.
            let _ = link_up_tx.send(Some(generation));
            link_tx.send(Input::LinkUp { gen: generation });

            tracing::info!(user = discord_user_id, "librespot OAuth session starting");
            let mut spirc_rx = Some(spirc_rx);
            let mut restarts: u32 = 0;
            loop {
                let access_token = { token_state.lock().0.clone() };
                let run_start = Instant::now();
                match SpotifyPlayer::run_with_token(
                    &config,
                    bridge.clone(),
                    transport_tx.clone(),
                    access_token,
                    &mut spirc_rx,
                )
                .await
                {
                    Ok(()) => tracing::info!(user = discord_user_id, "librespot session ended cleanly"),
                    Err(e) => {
                        tracing::warn!(user = discord_user_id, error = ?e, "librespot session ended with error")
                    }
                }
                // Only consecutive *fast* failures count toward giving up; a
                // session that ran for a while resets the budget.
                if run_start.elapsed() >= Duration::from_secs(MIN_STABLE_SESSION_SECS) {
                    restarts = 0;
                } else {
                    restarts += 1;
                }
                if restarts >= MAX_SESSION_RESTARTS {
                    tracing::warn!(user = discord_user_id, "librespot session gave up after repeated failures");
                    break;
                }
                // A fast-reconnect cycle, not a link-down: no armed-clearing,
                // no turn change (Input::LinkReconnecting is informational).
                link_tx.send(Input::LinkReconnecting { gen: generation });
                // Ask the refresher to rotate the token (in case the death
                // was an auth failure), then retry with whatever it
                // publishes.
                refresh_now.notify_one();
                tokio::time::sleep(Duration::from_secs(SESSION_RESTART_DELAY_SECS)).await;
            }

            // Give-up path: clear the slot only if this exact spawn still
            // owns it (a newer `switch`/`stop` may already have taken it),
            // and abort its refresher — leaving it detached would rotate the
            // token forever and race a future `switch`.
            let owned = {
                let mut lock = live_slot.lock().await;
                match lock.as_ref() {
                    Some(s) if s.generation == generation => lock.take(),
                    _ => None,
                }
            };
            if let Some(session) = owned {
                session.abort();
            }
            *spirc_cmd_tx_slot.lock() = None;
            let _ = link_up_tx.send(None);
            link_tx.send(Input::LinkDown { gen: generation });
        });

        *live = Some(LiveSession { discord_user_id, generation, handle, refresh_handle });
        tracing::info!(user = discord_user_id, name = %discord_name, "librespot session spawned");
    }

    /// Ensures a live session exists, starting one from the stored active
    /// account if needed. Returns the generation once the link is up.
    ///
    /// On-demand bring-up (C7): called from the interaction-handler task
    /// when a Spotify link is queued and no session is running — never
    /// from the player actor, which must never await something delivered
    /// into its own mailbox, and `LinkUp` is exactly that. The caller
    /// defers its Discord reply first, since the wait below can take up
    /// to 15s.
    ///
    /// Takes no lock across the wait: the up-front check and the final
    /// wait both go through [`Self::link_up_watch`], which — per the
    /// module docs — each generation's own task writes directly,
    /// bypassing `switch`'s lock entirely. The only lock touched here at
    /// all is the one `switch` itself takes internally, and that guard is
    /// dropped when `switch` returns, well before the wait begins.
    pub async fn ensure_session(&self, oauth: &SpotifyOAuth, store: &UserStore) -> EnsureOutcome {
        let mut rx = self.link_up_watch();
        if let Some(gen) = *rx.borrow() {
            return EnsureOutcome::Ready(gen);
        }

        let Some(user) = store.list().into_iter().find(|u| u.active) else {
            return EnsureOutcome::NoAccount;
        };
        let Ok(discord_user_id) = user.discord_user_id.parse::<u64>() else {
            return EnsureOutcome::Failed(format!(
                "unparseable stored discord user id: {}",
                user.discord_user_id
            ));
        };

        // Persist the rotated tokens exactly as `auto_start_stored_session`
        // does today: a refresh failure means the stored credentials are
        // stale or revoked, so there is nothing to retry here — the caller
        // reports it and the next fix is a fresh `/login`.
        let (access_token, refresh_token, expires_in) =
            match oauth.refresh_access_token(&user.refresh_token).await {
                Ok(t) => {
                    let mut updated = user.clone();
                    updated.access_token = t.access_token.clone();
                    if let Some(rt) = t.refresh_token.clone() {
                        updated.refresh_token = rt;
                    }
                    let _ = store.save(&updated);
                    (t.access_token, updated.refresh_token, t.expires_in)
                }
                Err(e) => return EnsureOutcome::Failed(format!("token refresh failed: {e}")),
            };

        self.switch(
            discord_user_id,
            user.discord_name.clone(),
            access_token,
            refresh_token,
            expires_in,
        )
        .await;

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(gen) = *rx.borrow() {
                return EnsureOutcome::Ready(gen);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return EnsureOutcome::Failed(
                    "timed out waiting for the Spotify session to connect".into(),
                );
            };
            match tokio::time::timeout(remaining, rx.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return EnsureOutcome::Failed("session link watch closed".into()),
                Err(_) => {
                    return EnsureOutcome::Failed(
                        "timed out waiting for the Spotify session to connect".into(),
                    )
                }
            }
        }
    }

    /// Stop the live session, but only if `owner` is the one running it — a
    /// bystander's `/logout` must not touch someone else's session.
    pub async fn stop(&self, owner: u64) {
        let mut live = self.live.lock().await;
        let is_owner = live.as_ref().is_some_and(|s| s.discord_user_id == owner);
        if !is_owner {
            return;
        }
        let session = live.take().expect("checked Some(_) owned by `owner` above");
        tracing::info!(user = owner, "aborting session (stop)");
        // Say goodbye first: a bare abort leaves the device listed (and
        // selected) in Spotify clients until their dealer times it out.
        let tx = self.spirc_cmd_tx.lock().take();
        if let Some(tx) = tx {
            if tx.send(SpircCommand::Shutdown).is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
        session.abort();
        let _ = self.link_up_tx.send(None);
        self.link_tx.send(Input::LinkDown { gen: session.generation });
    }
}
