use super::presence::run_presence_loop;
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
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
use serenity::builder::CreateCommandOption;
use serenity::client::{Client, Context, EventHandler};
use serenity::model::application::CommandOptionType;
use songbird::events::{Event, TrackEvent};
use songbird::SerenityInit;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

type ReadySignal = Result<(), String>;

/// An active librespot session spawned after OAuth login.
pub struct ActiveSession {
    pub discord_user_id: u64,
    pub spotify_name: String,
    pub handle: JoinHandle<()>,
}

struct Handler {
    guild_id: GuildId,
    channel_id: ChannelId,
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

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord bot connected");

        // Register slash commands for this guild
        match self.guild_id.set_commands(&ctx, register_commands()).await {
            Ok(cmds) => tracing::info!("registered {} slash commands", cmds.len()),
            Err(e) => tracing::warn!(error = ?e, "failed to register slash commands"),
        }

        let manager = match songbird::get(&ctx).await {
            Some(m) => m,
            None => {
                tracing::error!("songbird voice manager not registered");
                return;
            }
        };

        match manager.join(self.guild_id, self.channel_id).await {
            Ok(call) => {
                tracing::info!("joined voice channel");
                let mut call = call.lock().await;

                let reader = SimpleBridgeReader::new(
                    self.bridge.clone(),
                    self.prebuffer_samples,
                    self.prebuffer_wait,
                );
                let input = reader.into_input();

                let track_handle = call.play_only(input.into());
                let _ = track_handle.add_event(Event::Track(TrackEvent::Error), TrackErrorHandler);
                let _ = track_handle.add_event(Event::Track(TrackEvent::End), TrackErrorHandler);

                tracing::info!(track_uuid = ?track_handle.uuid(), "audio source connected to voice channel");

                if matches!(
                    configured_channel_kind(&ctx, self.channel_id).await,
                    Some(ChannelType::Stage)
                ) {
                    match self.channel_id.to_channel(&ctx).await {
                        Ok(Channel::Guild(channel)) => {
                            let builder = EditVoiceState::new().suppress(false);
                            match channel.edit_own_voice_state(&ctx, builder).await {
                                Ok(()) => tracing::info!("unsuppressed bot in stage channel"),
                                Err(error) => tracing::warn!(
                                    error = ?error,
                                    "failed to unsuppress bot in stage channel"
                                ),
                            }
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!(
                            channel_id = %self.channel_id,
                            error = ?error,
                            "failed to fetch stage channel after voice join"
                        ),
                    }
                }

                let _ = self.ready_tx.send(Ok(())).await;
            }
            Err(e) => {
                tracing::error!(error = ?e, "failed to join voice channel");
                let _ = self.ready_tx.send(Err(format!("{e:?}"))).await;
            }
        }

        let mut presence_rx = self.presence_rx.lock().await;
        if let Some(rx) = presence_rx.take() {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                run_presence_loop(ctx, rx).await;
            });
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let cmd = match interaction.command() {
            Some(c) => c,
            None => return,
        };

        // Extract the optional "code" string option
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
            "login" => {
                self.handle_login(&user_id, user_id_u64, &username, code_arg.as_deref())
                    .await
            }
            "logout" => self.handle_logout(&user_id, user_id_u64).await,
            "forget" => self.handle_forget(&user_id).await,
            "who" => self.handle_who().await,
            _ => return,
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(reply)
                .ephemeral(true),
        );

        if let Err(e) = cmd.create_response(&ctx, response).await {
            tracing::warn!(error = ?e, "failed to create interaction response");
        }
    }
}

impl Handler {
    /// Spawn a librespot session using an OAuth access_token.
    /// Stops any currently running session first.
    async fn spawn_session(
        &self,
        discord_user_id: u64,
        spotify_name: String,
        access_token: String,
    ) {
        let config = self.config.clone();
        let bridge = self.bridge.clone();
        let presence_tx = self.presence_tx.clone();
        let active_session = self.active_session.clone();

        // Abort any existing session first
        {
            let mut lock = active_session.lock().await;
            if let Some(old) = lock.take() {
                tracing::info!(
                    old_user = old.discord_user_id,
                    "aborting existing librespot session"
                );
                old.handle.abort();
            }
        }

        let active_session_for_task = active_session.clone();
        let spotify_name_clone = spotify_name.clone();
        let handle = tokio::spawn(async move {
            tracing::info!(user = discord_user_id, "librespot OAuth session starting");
            match SpotifyPlayer::run_with_token(&config, bridge, presence_tx, access_token).await {
                Ok(()) => {
                    tracing::info!(user = discord_user_id, "librespot session ended cleanly")
                }
                Err(e) => tracing::warn!(
                    user = discord_user_id,
                    error = ?e,
                    "librespot session ended with error"
                ),
            }
            // Clear active session when task exits naturally
            let mut lock = active_session_for_task.lock().await;
            if let Some(s) = lock.as_ref() {
                if s.discord_user_id == discord_user_id {
                    *lock = None;
                }
            }
        });

        let mut lock = active_session.lock().await;
        *lock = Some(ActiveSession {
            discord_user_id,
            spotify_name,
            handle,
        });
        tracing::info!(
            user = discord_user_id,
            spotify = %spotify_name_clone,
            "librespot session spawned"
        );
    }

    /// /login — no args: start OAuth or reactivate existing session.
    /// /login code:<url|token>: complete the OAuth flow.
    async fn handle_login(
        &self,
        user_id: &str,
        user_id_u64: u64,
        _discord_username: &str,
        code_arg: Option<&str>,
    ) -> String {
        // Check for existing credentials first
        if let Some(existing) = self.user_store.load(user_id) {
            // If no code provided and we have stored creds, reactivate
            if code_arg.is_none() {
                let oauth = match &self.oauth {
                    Some(o) => o.clone(),
                    None => {
                        // No OAuth configured — just flip active flag
                        let mut creds = existing.clone();
                        creds.active = true;
                        match self.user_store.save(&creds) {
                            Ok(()) => {
                                return format!(
                                    "Session reactivated as **{}**!",
                                    creds.spotify_username
                                )
                            }
                            Err(e) => {
                                tracing::error!("failed to reactivate session: {}", e);
                                return "Failed to reactivate session. Please try again."
                                    .to_string();
                            }
                        }
                    }
                };

                if existing.active {
                    // Already active — refresh token and restart session
                    match oauth.refresh_access_token(&existing.refresh_token).await {
                        Ok(new_token) => {
                            let mut creds = existing.clone();
                            creds.access_token = new_token.access_token.clone();
                            if let Some(rt) = new_token.refresh_token {
                                creds.refresh_token = rt;
                            }
                            let _ = self.user_store.save(&creds);
                            self.spawn_session(
                                user_id_u64,
                                existing.spotify_username.clone(),
                                new_token.access_token,
                            )
                            .await;
                            return format!(
                                "Session restarted for **{}**!",
                                existing.spotify_username
                            );
                        }
                        Err(e) => {
                            tracing::warn!("token refresh failed for reactivation: {}", e);
                            return format!(
                                "Already logged in as **{}** but couldn't refresh the token. \
                                 Use `/logout` then `/login` to re-authorize.",
                                existing.spotify_username
                            );
                        }
                    }
                } else {
                    // Inactive — reactivate and spawn
                    match oauth.refresh_access_token(&existing.refresh_token).await {
                        Ok(new_token) => {
                            let mut creds = existing.clone();
                            creds.active = true;
                            creds.access_token = new_token.access_token.clone();
                            if let Some(rt) = new_token.refresh_token {
                                creds.refresh_token = rt;
                            }
                            match self.user_store.save(&creds) {
                                Ok(()) => {
                                    self.spawn_session(
                                        user_id_u64,
                                        existing.spotify_username.clone(),
                                        new_token.access_token,
                                    )
                                    .await;
                                    tracing::info!(
                                        user = %user_id,
                                        spotify = %existing.spotify_username,
                                        "session reactivated"
                                    );
                                    return format!(
                                        "Session reactivated as **{}**!",
                                        existing.spotify_username
                                    );
                                }
                                Err(e) => {
                                    tracing::error!("failed to save reactivated session: {}", e);
                                    return "Failed to save session. Please try again.".to_string();
                                }
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
            None => {
                return "OAuth not configured. Set SPOTIFY_CLIENT_ID and SPOTIFY_CLIENT_SECRET in .env.".to_string();
            }
        };

        match code_arg {
            None => {
                // No code, no existing creds — start OAuth flow
                let state = generate_state();
                let url = oauth.auth_url(&state);
                format!(
                    "Connect your Spotify account:\n\n<{}>\n\nClick the link, authorize, then \
                     copy the full URL your browser tried to navigate to (it will fail with \
                     connection refused — that's expected). Run `/login code:<that URL>` to complete.",
                    url
                )
            }
            Some(raw) => {
                // Complete the flow
                let code = match SpotifyOAuth::extract_code(raw) {
                    Some(c) => c,
                    None => {
                        return "Couldn't extract a code from that input. Please paste the full redirect URL from your browser.".to_string();
                    }
                };

                match oauth.exchange_code(&code).await {
                    Ok(token) => {
                        let refresh_token = match token.refresh_token {
                            Some(rt) => rt,
                            None => {
                                return "Spotify didn't return a refresh token. Please try again."
                                    .to_string();
                            }
                        };

                        let display_name =
                            match oauth.get_user_profile(&token.access_token).await {
                                Ok(name) => name,
                                Err(e) => {
                                    tracing::warn!("failed to fetch Spotify profile: {}", e);
                                    "Unknown".to_string()
                                }
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
                                tracing::info!(
                                    user = %user_id,
                                    spotify = %display_name,
                                    "OAuth login successful"
                                );
                                self.spawn_session(
                                    user_id_u64,
                                    display_name.clone(),
                                    token.access_token,
                                )
                                .await;
                                format!(
                                    "Logged in as **{}**! Spotify session started.",
                                    display_name
                                )
                            }
                            Err(e) => {
                                tracing::error!("failed to save credentials: {}", e);
                                "Failed to save credentials. Please try again.".to_string()
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("OAuth code exchange failed: {}", e);
                        "Failed to exchange code with Spotify. The code may have expired — \
                         run `/login` to start over."
                            .to_string()
                    }
                }
            }
        }
    }

    /// /logout — soft deactivate (keeps creds stored) and abort active session
    async fn handle_logout(&self, user_id: &str, user_id_u64: u64) -> String {
        // Abort the active session if it belongs to this user
        {
            let mut lock = self.active_session.lock().await;
            if let Some(session) = lock.as_ref() {
                if session.discord_user_id == user_id_u64 {
                    session.handle.abort();
                    *lock = None;
                    tracing::info!(user = %user_id, "active librespot session aborted");
                }
            }
        }

        // Send idle presence
        let _ = self.presence_tx.send(PresenceUpdate::Idle);

        match self.user_store.deactivate(user_id) {
            Ok(true) => {
                tracing::info!(user = %user_id, "session deactivated");
                "Session deactivated. Your credentials are kept — run `/login` to reactivate without re-authorizing.".to_string()
            }
            Ok(false) => "You don't have an active session.".to_string(),
            Err(e) => {
                tracing::error!("failed to deactivate session: {}", e);
                "Failed to deactivate session.".to_string()
            }
        }
    }

    /// /forget — hard delete stored credentials
    async fn handle_forget(&self, user_id: &str) -> String {
        match self.user_store.remove(user_id) {
            Ok(true) => {
                tracing::info!(user = %user_id, "credentials forgotten");
                "Credentials permanently deleted. Run `/login` to connect again.".to_string()
            }
            Ok(false) => "No stored credentials to delete.".to_string(),
            Err(e) => {
                tracing::error!("failed to delete credentials: {}", e);
                "Failed to delete credentials.".to_string()
            }
        }
    }

    /// /who — show active session info (reads shared state, not per-user store)
    async fn handle_who(&self) -> String {
        let lock = self.active_session.lock().await;
        match lock.as_ref() {
            Some(session) => format!(
                "Active session: **{}** (Discord user {})",
                session.spotify_name, session.discord_user_id
            ),
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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
        let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

        let (ready_tx, ready_rx) = mpsc::channel(1);

        let prebuffer_samples =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_wait =
            std::time::Duration::from_secs_f32((config.prebuffer_seconds + 0.5).clamp(0.0, 5.0));

        let active_session: Arc<Mutex<Option<ActiveSession>>> = Arc::new(Mutex::new(None));

        let handler = Handler {
            guild_id: GuildId::new(config.discord_guild_id),
            channel_id: ChannelId::new(config.discord_channel_id),
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
        };

        let token = config.discord_token.clone();
        let client = Client::builder(&token, intents)
            .event_handler(handler)
            .register_songbird()
            .await?;

        Ok(Self {
            client,
            ready_rx,
            active_session,
        })
    }

    pub async fn start_background(
        mut self,
    ) -> Result<
        (
            mpsc::Receiver<ReadySignal>,
            Arc<Mutex<Option<ActiveSession>>>,
        ),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let active_session = self.active_session.clone();
        tokio::spawn(async move {
            if let Err(e) = self.client.start().await {
                tracing::error!(error = ?e, "discord client error");
            }
        });

        Ok((self.ready_rx, active_session))
    }
}
