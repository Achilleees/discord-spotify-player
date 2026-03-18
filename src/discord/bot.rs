use super::presence::run_presence_loop;
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio::generate_join_sound;
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::oauth::SpotifyOAuth;
use crate::presence::PresenceUpdate;
use crate::spotify::SpotifyPlayer;
use crate::users::{UserCredentials, UserStore};
use serenity::all::{
    Channel, ChannelId, ChannelType, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditVoiceState, GatewayIntents, GuildId, Interaction, Ready,
};
use serenity::async_trait;
use serenity::builder::{CreateActionRow, CreateButton, CreateCommandOption, CreateMessage};
use serenity::client::{Client, Context, EventHandler};
use serenity::model::application::{ButtonStyle, CommandOptionType};
use serenity::model::voice::VoiceState;
use songbird::events::{Event, TrackEvent};
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::SerenityInit;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type ReadySignal = Result<(), String>;

pub struct ActiveSession {
    pub discord_user_id: u64,
    pub spotify_name: String,
    pub access_token: String,
    pub handle: JoinHandle<()>,
}

struct Handler {
    guild_id: GuildId,
    channel_id: ChannelId,
    text_channel_id: ChannelId,
    bridge: Arc<AudioBridge>,
    config: Arc<Config>,
    ready_tx: mpsc::Sender<ReadySignal>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    user_store: Arc<UserStore>,
    oauth: Option<Arc<SpotifyOAuth>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    track_handle: Arc<Mutex<Option<TrackHandle>>>,
}

async fn configured_channel_kind(ctx: &Context, channel_id: ChannelId) -> Option<ChannelType> {
    match channel_id.to_channel(ctx).await {
        Ok(Channel::Guild(channel)) => Some(channel.kind),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(channel_id = %channel_id, error = ?error, "failed to resolve configured channel");
            None
        }
    }
}

fn register_commands() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("login")
            .description("Connect your Spotify account (or reactivate existing session)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "code",
                    "Paste the redirect URL (or code) after authorizing",
                )
                .required(false),
            ),
        CreateCommand::new("logout")
            .description("Deactivate your Spotify session (credentials kept for quick re-login)"),
        CreateCommand::new("forget")
            .description("Permanently delete your stored Spotify credentials"),
        CreateCommand::new("who")
            .description("Show whose Spotify account is currently active"),
    ]
}

struct CursorSource(std::io::Cursor<Vec<u8>>);

impl Read for CursorSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for CursorSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl songbird::input::core::io::MediaSource for CursorSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.0.get_ref().len() as u64)
    }
}

async fn play_join_sound_then_bridge(
    call_lock: Arc<tokio::sync::Mutex<songbird::Call>>,
    bridge: Arc<AudioBridge>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
) {
    let join_samples = generate_join_sound();
    let stereo_f32: Vec<f32> = join_samples
        .iter()
        .flat_map(|&s| {
            let f = s as f32 / i16::MAX as f32;
            [f, f]
        })
        .collect();
    let bytes: Vec<u8> = stereo_f32.iter().flat_map(|s| s.to_le_bytes()).collect();
    let duration_secs = join_samples.len() as f64 / 44100.0;

    {
        let mut call = call_lock.lock().await;
        let boop_source = CursorSource(std::io::Cursor::new(bytes));
        let raw = RawAdapter::new(boop_source, SAMPLE_RATE, CHANNELS);
        let input: Input = raw.into();
        let _boop_handle = call.play_only(input.into());
    }

    tokio::time::sleep(std::time::Duration::from_secs_f64(duration_secs + 0.1)).await;

    let reader = SimpleBridgeReader::new(bridge, prebuffer_samples, prebuffer_wait);
    let input = reader.into_input();
    let mut call = call_lock.lock().await;
    let track_handle = call.play_only(input.into());
    let _ = track_handle.add_event(Event::Track(TrackEvent::Error), TrackErrorHandler);
    let _ = track_handle.add_event(Event::Track(TrackEvent::End), TrackErrorHandler);
    tracing::info!(track_uuid = ?track_handle.uuid(), "bridge reader connected after join sound");
    // Start paused; will resume on first Playing event
    let _ = track_handle.pause();
    let mut lock = track_handle_store.lock().unwrap_or_else(|e| e.into_inner());
    *lock = Some(track_handle);
}

async fn post_or_update_controls(
    ctx: &Context,
    text_channel_id: ChannelId,
    bot_id: serenity::model::id::UserId,
) {
    let controls_text = concat!(
        "🎛️ **Spotibot Controls**\n",
        "Use `/login` to take over the session\n",
        "Use `/logout` to release\n",
        "Use `/who` to see who's playing"
    );

    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("ctrl_prev").label("⏮").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_pause_toggle").label("⏸").style(ButtonStyle::Secondary),
        CreateButton::new("ctrl_next").label("⏭").style(ButtonStyle::Secondary),
    ]);

    let pins = match text_channel_id.pins(ctx).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = ?e, "failed to fetch pins");
            vec![]
        }
    };

    let existing_pin = pins.iter().find(|msg| msg.author.id == bot_id);

    if let Some(pinned_msg) = existing_pin {
        use serenity::builder::EditMessage;
        let edit = EditMessage::new().content(controls_text).components(vec![buttons]);
        match text_channel_id.edit_message(ctx, pinned_msg.id, edit).await {
            Ok(_) => tracing::info!("updated existing pinned controls message"),
            Err(e) => tracing::warn!(error = ?e, "failed to edit pinned controls message"),
        }
    } else {
        let msg = CreateMessage::new().content(controls_text).components(vec![buttons]);
        match text_channel_id.send_message(ctx, msg).await {
            Ok(m) => {
                if let Err(e) = m.pin(ctx).await {
                    tracing::warn!(error = ?e, "failed to pin controls message");
                } else {
                    tracing::info!("posted and pinned controls message");
                }
            }
            Err(e) => tracing::warn!(error = ?e, "failed to post controls message"),
        }
    }
}

async fn spotify_playback_command(access_token: &str, method: &str, endpoint: &str) {
    let client = reqwest::Client::new();
    let url = format!("https://api.spotify.com/v1/me/player/{}", endpoint);
    let req = match method {
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        _ => return,
    };
    match req
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Length", "0")
        .send()
        .await
    {
        Ok(r) => tracing::info!(status = r.status().as_u16(), endpoint, "spotify API call"),
        Err(e) => tracing::warn!(error = ?e, endpoint, "spotify API call failed"),
    }
}

async fn run_presence_loop_with_track(
    ctx: Context,
    mut rx: mpsc::UnboundedReceiver<PresenceUpdate>,
    track_handle_store: Arc<Mutex<Option<TrackHandle>>>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
    text_channel_id: ChannelId,
) {
    let (fwd_tx, fwd_rx) = mpsc::unbounded_channel::<PresenceUpdate>();
    let ctx_presence = ctx.clone();
    tokio::spawn(async move {
        run_presence_loop(ctx_presence, fwd_rx).await;
    });

    let mut last_track_key: Option<String> = None;

    while let Some(update) = rx.recv().await {
        let _ = fwd_tx.send(update.clone());

        // Speaking ring control via track handle
        {
            let lock = track_handle_store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(handle) = lock.as_ref() {
                match &update {
                    PresenceUpdate::Playing { .. } => { let _ = handle.play(); }
                    PresenceUpdate::Paused | PresenceUpdate::Idle => { let _ = handle.pause(); }
                }
            }
        }

        // Now-playing message on new track
        if let PresenceUpdate::Playing { title, artist } = &update {
            let track_key = format!("{} — {}", title, artist);
            if last_track_key.as_deref() != Some(&track_key) {
                last_track_key = Some(track_key.clone());
                let spotify_name = {
                    let lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().map(|s| s.spotify_name.clone()).unwrap_or_default()
                };
                let msg_content = if spotify_name.is_empty() {
                    format!("🎵 **{}** — {}", title, artist)
                } else {
                    format!("🎵 **{}** — {} *(via {})*", title, artist, spotify_name)
                };
                let ctx_msg = ctx.clone();
                let title_owned = title.clone();
                let artist_owned = artist.clone();
                tokio::spawn(async move {
                    let msg = CreateMessage::new()
                        .content(msg_content);
                    match text_channel_id.send_message(&ctx_msg, msg).await {
                        Ok(_) => tracing::info!(title = %title_owned, artist = %artist_owned, "now-playing message sent"),
                        Err(e) => tracing::warn!(error = ?e, "failed to send now-playing message"),
                    }
                });
            }
        } else if matches!(update, PresenceUpdate::Idle) {
            last_track_key = None;
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord bot connected");

        match self.guild_id.set_commands(&ctx, register_commands()).await {
            Ok(cmds) => tracing::info!("registered {} slash commands", cmds.len()),
            Err(e) => tracing::warn!(error = ?e, "failed to register slash commands"),
        }

        let manager = match songbird::get(&ctx).await {
            Some(m) => m,
            None => { tracing::error!("songbird voice manager not registered"); return; }
        };

        match manager.join(self.guild_id, self.channel_id).await {
            Ok(call) => {
                tracing::info!("joined voice channel");

                if matches!(
                    configured_channel_kind(&ctx, self.channel_id).await,
                    Some(ChannelType::Stage)
                ) {
                    match self.channel_id.to_channel(&ctx).await {
                        Ok(Channel::Guild(channel)) => {
                            let builder = EditVoiceState::new().suppress(false);
                            match channel.edit_own_voice_state(&ctx, builder).await {
                                Ok(()) => tracing::info!("unsuppressed bot in stage channel"),
                                Err(error) => tracing::warn!(error = ?error, "failed to unsuppress bot in stage channel"),
                            }
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!(channel_id = %self.channel_id, error = ?error, "failed to fetch stage channel after voice join"),
                    }
                }

                let _ = self.ready_tx.send(Ok(())).await;

                let bridge = self.bridge.clone();
                let prebuffer_samples = self.prebuffer_samples;
                let prebuffer_wait = self.prebuffer_wait;
                let track_handle_store = self.track_handle.clone();
                tokio::spawn(play_join_sound_then_bridge(call, bridge, prebuffer_samples, prebuffer_wait, track_handle_store));

                let ctx_for_controls = ctx.clone();
                let text_channel_id = self.text_channel_id;
                let bot_id = ready.user.id;
                tokio::spawn(async move {
                    post_or_update_controls(&ctx_for_controls, text_channel_id, bot_id).await;
                });
            }
            Err(e) => {
                tracing::error!(error = ?e, "failed to join voice channel");
                let _ = self.ready_tx.send(Err(format!("{e:?}"))).await;
            }
        }

        let mut presence_rx = self.presence_rx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rx) = presence_rx.take() {
            let ctx_presence = ctx.clone();
            let track_handle_store = self.track_handle.clone();
            let active_session = self.active_session.clone();
            let text_channel_id = self.text_channel_id;
            tokio::spawn(async move {
                run_presence_loop_with_track(ctx_presence, rx, track_handle_store, active_session, text_channel_id).await;
            });
        }
    }

    async fn voice_state_update(&self, ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        if new.guild_id != Some(self.guild_id) {
            return;
        }

        // Extract what we need from cache before any await points (CacheRef is !Send)
        let humans_in_channel = {
            let bot_id = ctx.cache.current_user().id;
            match self.guild_id.to_guild_cached(&ctx) {
                Some(guild) => guild
                    .voice_states
                    .values()
                    .filter(|vs| vs.channel_id == Some(self.channel_id))
                    .filter(|vs| vs.user_id != bot_id)
                    .filter(|vs| guild.members.get(&vs.user_id).map(|m| !m.user.bot).unwrap_or(true))
                    .count(),
                None => return,
            }
        };

        tracing::debug!(humans_in_channel, "voice state checked");

        if humans_in_channel == 0 {
            let has_session = {
                let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                lock.is_some()
            };

            if has_session {
                tracing::info!("voice channel empty — auto-logout triggered");

                {
                    let mut lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(session) = lock.take() {
                        session.handle.abort();
                        tracing::info!(user = session.discord_user_id, "auto-aborted session (empty channel)");
                    }
                }

                let _ = self.presence_tx.send(PresenceUpdate::Idle);

                if let Some(manager) = songbird::get(&ctx).await {
                    let _ = manager.leave(self.guild_id).await;
                    tracing::info!("bot left voice channel (channel empty)");
                }

                for user in self.user_store.list() {
                    if user.active {
                        let _ = self.user_store.deactivate(&user.discord_user_id);
                    }
                }
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        tracing::info!("interaction_create fired");

        if let Interaction::Component(component) = &interaction {
            let custom_id = component.data.custom_id.as_str();
            tracing::info!(custom_id, "button interaction received");

            let access_token = {
                let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
                lock.as_ref().map(|s| s.access_token.clone())
            };

            let reply_content = if let Some(token) = access_token {
                match custom_id {
                    "ctrl_prev" => {
                        spotify_playback_command(&token, "POST", "previous").await;
                        "⏮ Previous"
                    }
                    "ctrl_next" => {
                        spotify_playback_command(&token, "POST", "next").await;
                        "⏭ Skipped"
                    }
                    "ctrl_pause_toggle" => {
                        let handle_clone = {
                            let lock = self.track_handle.lock().unwrap_or_else(|e| e.into_inner());
                            lock.as_ref().cloned()
                        };
                        let is_paused = if let Some(h) = handle_clone {
                            h.get_info().await
                                .map(|info| info.playing == songbird::tracks::PlayMode::Pause)
                                .unwrap_or(false)
                        } else {
                            false
                        };
                        if is_paused {
                            spotify_playback_command(&token, "PUT", "play").await;
                            "▶ Resumed"
                        } else {
                            spotify_playback_command(&token, "PUT", "pause").await;
                            "⏸ Paused"
                        }
                    }
                    _ => "Unknown button",
                }
            } else {
                "No active Spotify session"
            };

            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new().content(reply_content).ephemeral(true),
            );
            if let Err(e) = component.create_response(&ctx, response).await {
                tracing::warn!(error = ?e, "failed to respond to button interaction");
            }
            return;
        }

        let cmd = match interaction.command() {
            Some(c) => c,
            None => { tracing::warn!("interaction was not a command or component"); return; }
        };
        tracing::info!(command = %cmd.data.name, "processing slash command");

        let code_arg: Option<String> = cmd
            .data
            .options
            .iter()
            .find(|o| o.name == "code")
            .and_then(|o| {
                if let serenity::model::application::CommandDataOptionValue::String(s) = &o.value {
                    Some(s.clone())
                } else {
                    None
                }
            });

        let user_id = cmd.user.id.to_string();
        let user_id_u64 = cmd.user.id.get();
        let username = cmd.user.name.clone();

        let reply = match cmd.data.name.as_str() {
            "login" => self.handle_login(&user_id, user_id_u64, &username, code_arg.as_deref()).await,
            "logout" => self.handle_logout(&user_id, user_id_u64).await,
            "forget" => self.handle_forget(&user_id).await,
            "who" => self.handle_who().await,
            _ => return,
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(reply).ephemeral(true),
        );

        if let Err(e) = cmd.create_response(&ctx, response).await {
            tracing::warn!(error = ?e, "failed to create interaction response");
        }
    }
}

impl Handler {
    async fn spawn_session(
        &self,
        discord_user_id: u64,
        spotify_name: String,
        access_token: String,
        refresh_token: String,
    ) {
        let config = self.config.clone();
        let bridge = self.bridge.clone();
        let presence_tx = self.presence_tx.clone();
        let active_session = self.active_session.clone();
        let oauth_for_task = self.oauth.clone();
        let user_store_for_task = self.user_store.clone();
        let user_id_str = discord_user_id.to_string();
        let mut refresh_token = refresh_token;
        let mut access_token = access_token;

        {
            let mut lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = lock.take() {
                tracing::info!(old_user = old.discord_user_id, "aborting existing librespot session");
                old.handle.abort();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let active_session_for_task = active_session.clone();
        let spotify_name_clone = spotify_name.clone();
        let access_token_for_store = access_token.clone();
        let handle = tokio::spawn(async move {
            tracing::info!(user = discord_user_id, "librespot OAuth session starting");
            loop {
                match SpotifyPlayer::run_with_token(&config, bridge.clone(), presence_tx.clone(), access_token.clone()).await {
                    Ok(()) => tracing::info!(user = discord_user_id, "librespot session ended cleanly"),
                    Err(e) => tracing::warn!(user = discord_user_id, error = ?e, "librespot session ended with error"),
                }
                tracing::info!(user = discord_user_id, "attempting token refresh and reconnect in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let oauth_ref = match &oauth_for_task {
                    Some(o) => o.clone(),
                    None => { tracing::warn!(user = discord_user_id, "no OAuth client"); break; }
                };
                match oauth_ref.refresh_access_token(&refresh_token).await {
                    Ok(new_token) => {
                        tracing::info!(user = discord_user_id, "token refreshed, reconnecting");
                        if let Some(rt) = new_token.refresh_token.clone() { refresh_token = rt; }
                        access_token = new_token.access_token;
                        if let Some(mut creds) = user_store_for_task.load(&user_id_str) {
                            creds.access_token = access_token.clone();
                            creds.refresh_token = refresh_token.clone();
                            let _ = user_store_for_task.save(&creds);
                        }
                        {
                            let mut lock = active_session_for_task.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(s) = lock.as_mut() {
                                if s.discord_user_id == discord_user_id {
                                    s.access_token = access_token.clone();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(user = discord_user_id, error = ?e, "token refresh failed");
                        break;
                    }
                }
            }
            let mut lock = active_session_for_task.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = lock.as_ref() {
                if s.discord_user_id == discord_user_id { *lock = None; }
            }
        });

        let mut lock = active_session.lock().unwrap_or_else(|e| e.into_inner());
        *lock = Some(ActiveSession {
            discord_user_id,
            spotify_name,
            access_token: access_token_for_store,
            handle,
        });
        tracing::info!(user = discord_user_id, spotify = %spotify_name_clone, "librespot session spawned");
    }

    async fn handle_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        _discord_username: &str,
        code_arg: Option<&str>,
    ) -> String {
        if let Some(existing) = self.user_store.load(user_id) {
            if code_arg.is_none() {
                let oauth = match &self.oauth {
                    Some(o) => o.clone(),
                    None => {
                        let mut creds = existing.clone();
                        creds.active = true;
                        match self.user_store.save(&creds) {
                            Ok(()) => return format!("Session reactivated as **{}**!", creds.spotify_username),
                            Err(e) => { tracing::error!("failed to reactivate session: {}", e); return "Failed to reactivate session. Please try again.".to_string(); }
                        }
                    }
                };

                if existing.active {
                    match oauth.refresh_access_token(&existing.refresh_token).await {
                        Ok(new_token) => {
                            let mut creds = existing.clone();
                            creds.access_token = new_token.access_token.clone();
                            if let Some(rt) = new_token.refresh_token { creds.refresh_token = rt; }
                            let _ = self.user_store.save(&creds);
                            self.spawn_session(user_id_u64, existing.spotify_username.clone(), new_token.access_token, creds.refresh_token.clone()).await;
                            return format!("Session restarted for **{}**!", existing.spotify_username);
                        }
                        Err(e) => {
                            tracing::warn!("token refresh failed for reactivation: {}", e);
                            return format!("Already logged in as **{}** but couldn't refresh the token. Use `/logout` then `/login` to re-authorize.", existing.spotify_username);
                        }
                    }
                } else {
                    match oauth.refresh_access_token(&existing.refresh_token).await {
                        Ok(new_token) => {
                            let mut creds = existing.clone();
                            creds.active = true;
                            creds.access_token = new_token.access_token.clone();
                            if let Some(rt) = new_token.refresh_token { creds.refresh_token = rt; }
                            match self.user_store.save(&creds) {
                                Ok(()) => {
                                    self.spawn_session(user_id_u64, existing.spotify_username.clone(), new_token.access_token, creds.refresh_token.clone()).await;
                                    tracing::info!(user = %user_id, spotify = %existing.spotify_username, "session reactivated");
                                    return format!("Session reactivated as **{}**!", existing.spotify_username);
                                }
                                Err(e) => { tracing::error!("failed to save reactivated session: {}", e); return "Failed to save session. Please try again.".to_string(); }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("token refresh failed during reactivation: {}", e);
                            return "Couldn't refresh your Spotify token. Please run `/login` to re-authorize.".to_string();
                        }
                    }
                }
            }
        }

        let oauth = match &self.oauth {
            Some(o) => o.clone(),
            None => return "OAuth not configured. Set SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET in .env.".to_string(),
        };

        match code_arg {
            None => {
                let state = generate_state();
                let url = oauth.auth_url(&state);
                format!("Connect your Spotify account:

<{}>

Click the link, authorize, then copy the full URL your browser tried to navigate to (it will fail with connection refused — that's expected). Run `/login code:<that URL>` to complete.", url)
            }
            Some(raw) => {
                let code = match SpotifyOAuth::extract_code(raw) {
                    Some(c) => c,
                    None => return "Couldn't extract a code from that input. Please paste the full redirect URL from your browser.".to_string(),
                };

                match oauth.exchange_code(&code).await {
                    Ok(token) => {
                        let refresh_token = match token.refresh_token {
                            Some(rt) => rt,
                            None => return "Spotify didn't return a refresh token. Please try again.".to_string(),
                        };

                        let display_name = match oauth.get_user_profile(&token.access_token).await {
                            Ok(name) => name,
                            Err(e) => { tracing::warn!("failed to fetch Spotify profile: {}", e); "Unknown".to_string() }
                        };

                        let creds = UserCredentials {
                            discord_user_id: user_id.to_string(),
                            spotify_username: display_name.clone(),
                            access_token: token.access_token.clone(),
                            refresh_token,
                            paired_at: unix_timestamp_str(),
                            active: true,
                        };

                        match self.user_store.save(&creds) {
                            Ok(()) => {
                                tracing::info!(user = %user_id, spotify = %display_name, "OAuth login successful");
                                self.spawn_session(user_id_u64, display_name.clone(), token.access_token, creds.refresh_token.clone()).await;
                                format!("Logged in as **{}**! Spotify session started.", display_name)
                            }
                            Err(e) => { tracing::error!("failed to save credentials: {}", e); "Failed to save credentials. Please try again.".to_string() }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("OAuth code exchange failed: {}", e);
                        "Failed to exchange code with Spotify. The code may have expired — run `/login` to start over.".to_string()
                    }
                }
            }
        }
    }

    async fn handle_logout(&self, user_id: &str, user_id_u64: u64) -> String {
        {
            let mut lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = lock.as_ref() {
                if session.discord_user_id == user_id_u64 {
                    session.handle.abort();
                    *lock = None;
                    tracing::info!(user = %user_id, "active librespot session aborted");
                }
            }
        }
        let _ = self.presence_tx.send(PresenceUpdate::Idle);
        match self.user_store.deactivate(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "session deactivated"); "Session deactivated. Your credentials are kept — run `/login` to reactivate without re-authorizing.".to_string() }
            Ok(false) => "You don't have an active session.".to_string(),
            Err(e) => { tracing::error!("failed to deactivate session: {}", e); "Failed to deactivate session.".to_string() }
        }
    }

    async fn handle_forget(&self, user_id: &str) -> String {
        match self.user_store.remove(user_id) {
            Ok(true) => { tracing::info!(user = %user_id, "credentials forgotten"); "Credentials permanently deleted. Run `/login` to connect again.".to_string() }
            Ok(false) => "No stored credentials to delete.".to_string(),
            Err(e) => { tracing::error!("failed to delete credentials: {}", e); "Failed to delete credentials.".to_string() }
        }
    }

    async fn handle_who(&self) -> String {
        tracing::info!("handle_who: attempting lock");
        let lock = self.active_session.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("handle_who: lock acquired");
        match lock.as_ref() {
            Some(session) => format!("Active session: **{}** (Discord user {})", session.spotify_name, session.discord_user_id),
            None => "No active Spotify session. Run `/login` to connect.".to_string(),
        }
    }
}

fn generate_state() -> String {
    use rand::distr::SampleString;
    rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 16)
}

fn unix_timestamp_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("unix:{}", secs)
}

pub struct DiscordBot {
    client: Client,
    ready_rx: mpsc::Receiver<ReadySignal>,
    active_session: Arc<Mutex<Option<ActiveSession>>>,
}

impl DiscordBot {
    pub async fn new(
        config: Arc<Config>,
        bridge: Arc<AudioBridge>,
        presence_rx: mpsc::UnboundedReceiver<PresenceUpdate>,
        presence_tx: mpsc::UnboundedSender<PresenceUpdate>,
        user_store: Arc<UserStore>,
        oauth: Option<Arc<SpotifyOAuth>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_VOICE_STATES
            | GatewayIntents::GUILD_MEMBERS;

        let (ready_tx, ready_rx) = mpsc::channel(1);

        let prebuffer_samples =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_wait =
            std::time::Duration::from_secs_f32((config.prebuffer_seconds + 0.5).clamp(0.0, 5.0));

        let active_session: Arc<Mutex<Option<ActiveSession>>> = Arc::new(Mutex::new(None));
        let track_handle: Arc<Mutex<Option<TrackHandle>>> = Arc::new(Mutex::new(None));

        let handler = Handler {
            guild_id: GuildId::new(config.discord_guild_id),
            channel_id: ChannelId::new(config.discord_channel_id),
            text_channel_id: ChannelId::new(config.discord_text_channel_id),
            bridge,
            config: config.clone(),
            ready_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            presence_tx,
            prebuffer_samples,
            prebuffer_wait,
            user_store,
            oauth,
            active_session: active_session.clone(),
            track_handle,
        };

        let token = config.discord_token.clone();
        let client = Client::builder(&token, intents)
            .event_handler(handler)
            .register_songbird()
            .await?;

        Ok(Self { client, ready_rx, active_session })
    }

    pub async fn start_background(
        mut self,
    ) -> Result<(mpsc::Receiver<ReadySignal>, Arc<Mutex<Option<ActiveSession>>>), Box<dyn std::error::Error + Send + Sync>> {
        let active_session = self.active_session.clone();
        tokio::spawn(async move {
            if let Err(e) = self.client.start().await {
                tracing::error!(error = ?e, "discord client error");
            }
        });
        Ok((self.ready_rx, active_session))
    }
}
