use super::presence::run_presence_loop;
use super::voice::{SimpleBridgeReader, TrackErrorHandler, CHANNELS, SAMPLE_RATE};
use crate::audio_bridge::AudioBridge;
use crate::config::Config;
use crate::presence::PresenceUpdate;
use serenity::all::{
    Channel, ChannelId, ChannelType, EditVoiceState, GatewayIntents, GuildId, Ready,
};
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};
use songbird::events::{Event, TrackEvent};
use songbird::SerenityInit;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

type ReadySignal = Result<(), String>;

struct Handler {
    guild_id: GuildId,
    channel_id: ChannelId,
    bridge: Arc<AudioBridge>,
    ready_tx: mpsc::Sender<ReadySignal>,
    presence_rx: Mutex<Option<mpsc::UnboundedReceiver<PresenceUpdate>>>,
    prebuffer_samples: usize,
    prebuffer_wait: std::time::Duration,
}

fn is_dave_required_error(error_text: &str) -> bool {
    error_text.contains("4017") && error_text.to_ascii_lowercase().contains("dave")
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

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord bot connected");

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
                let error_text = format!("{e:?}");
                let channel_kind = configured_channel_kind(&ctx, self.channel_id).await;

                tracing::error!(error = ?e, "failed to join voice channel");

                if is_dave_required_error(&error_text)
                    && !matches!(channel_kind, Some(ChannelType::Stage))
                {
                    tracing::error!(
                        channel_id = %self.channel_id,
                        "discord now requires dave/e2ee for non-stage voice channels; use a stage channel until songbird adds dave support"
                    );
                }

                let _ = self.ready_tx.send(Err(error_text)).await;
            }
        }

        // Take the receiver exactly once; the spawned task owns it from here.
        let mut presence_rx = self.presence_rx.lock().await;
        if let Some(rx) = presence_rx.take() {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                run_presence_loop(ctx, rx).await;
            });
        }
    }
}

pub struct DiscordBot {
    client: Client,
    ready_rx: mpsc::Receiver<ReadySignal>,
}

impl DiscordBot {
    pub async fn new(
        config: &Config,
        bridge: Arc<AudioBridge>,
        presence_rx: mpsc::UnboundedReceiver<PresenceUpdate>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
        let (ready_tx, ready_rx) = mpsc::channel(1);

        let prebuffer_samples =
            (config.prebuffer_seconds * SAMPLE_RATE as f32) as usize * CHANNELS as usize;
        let prebuffer_wait =
            std::time::Duration::from_secs_f32((config.prebuffer_seconds + 0.5).clamp(0.0, 5.0));

        let handler = Handler {
            guild_id: GuildId::new(config.discord_guild_id),
            channel_id: ChannelId::new(config.discord_channel_id),
            bridge,
            ready_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            prebuffer_samples,
            prebuffer_wait,
        };

        let client = Client::builder(&config.discord_token, intents)
            .event_handler(handler)
            .register_songbird()
            .await?;

        Ok(Self { client, ready_rx })
    }

    pub async fn start_background(
        mut self,
    ) -> Result<mpsc::Receiver<ReadySignal>, Box<dyn std::error::Error + Send + Sync>> {
        tokio::spawn(async move {
            if let Err(e) = self.client.start().await {
                tracing::error!(error = ?e, "discord client error");
            }
        });

        Ok(self.ready_rx)
    }
}
