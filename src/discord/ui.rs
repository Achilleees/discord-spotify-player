//! Single owner of the now-playing/controls card.
//!
//! Every post/edit/delete against the channel's now-playing card goes
//! through one task and one mailbox (`UiMsg`), keyed on one `card_id` —
//! both the Spotify baseline and the priority (YouTube/file) queue send
//! here rather than touching the channel directly, so the two playback
//! sources can never race each other's post/delete.

use super::bot::SpotifyTrackInfo;
use crate::queue::{MediaSource, QueueItem};
use serenity::all::ChannelId;
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateMessage, EditMessage};
use serenity::client::Context;
use serenity::model::application::ButtonStyle;
use serenity::model::id::MessageId;
use serenity::model::Timestamp;
use tokio::sync::mpsc;

/// What to render on the now-playing card.
#[derive(Debug, Clone)]
pub enum CardView {
    Spotify {
        title: String,
        artist: String,
        track_id: String,
        album_art_url: Option<String>,
        dj_name: String,
    },
    Queued { item: QueueItem },
}

/// What to render on a history embed — the same two shapes as `CardView`,
/// for a track/item that has just been superseded.
#[derive(Debug, Clone)]
/// `Spotify` is posted automatically from the previous card; the explicit
/// variant is wired in C5 when the actor owns card triggering.
#[allow(dead_code)]
pub enum HistoryView {
    Spotify {
        title: String,
        artist: String,
        track_id: String,
        album_art_url: Option<String>,
        dj_name: String,
    },
    Queued { item: QueueItem },
}

/// Messages the UI task accepts.
pub enum UiMsg {
    /// A new track/item started. Posts history for whatever this
    /// supersedes when both are Spotify tracks (a Queued predecessor's
    /// history always arrives via an explicit `History` message instead,
    /// since a queue item's completion doesn't necessarily coincide with
    /// whatever plays next); deletes the old card; posts the new one.
    NowPlaying(CardView),
    /// Post a history embed immediately, independent of the current card.
    History(HistoryView),
    /// Nothing is playing: delete the current card and post the idle
    /// controls card. Media discovery does not require Spotify login.
    Idle { account: Option<String> },
    /// Update the current card's pause state and controls in place.
    Buttons { paused: bool },
    /// The DJ name changed (login, logout, forget). The current card is
    /// edited with the new name — a media item keeps its card through
    /// an account change; only with no card up does this post the idle
    /// controls.
    AccountChanged(Option<String>),
}

#[derive(Default)]
struct CardState {
    view: Option<CardView>,
    account: Option<String>,
    paused: bool,
    started_at: Option<Timestamp>,
}

impl CardState {
    /// Update desired rendering once. Retrying a card never repeats history.
    fn apply(&mut self, message: UiMsg) -> Option<HistoryView> {
        match message {
            UiMsg::NowPlaying(view) => {
                self.paused = false;
                self.started_at = Some(Timestamp::now());
                let both_spotify = matches!(&view, CardView::Spotify { .. });
                let previous = self.view.replace(view);
                if both_spotify {
                    if let Some(CardView::Spotify { title, artist, track_id, album_art_url, dj_name }) = previous {
                        return Some(HistoryView::Spotify { title, artist, track_id, album_art_url, dj_name });
                    }
                }
            }
            UiMsg::Idle { account } => {
                self.view = None;
                self.account = account;
                self.paused = false;
                self.started_at = None;
            }
            UiMsg::Buttons { paused } => self.paused = paused,
            UiMsg::AccountChanged(account) => {
                if let Some(CardView::Spotify { dj_name, .. }) = &mut self.view {
                    *dj_name = account.clone().unwrap_or_default();
                }
                self.account = account;
            }
            UiMsg::History(history) => return Some(history),
        }
        None
    }

    fn embed(&self, media_available: bool) -> CreateEmbed {
        let mut embed = match &self.view {
            Some(CardView::Spotify { title, artist, track_id, album_art_url, dj_name }) => {
                let meta = SpotifyTrackInfo { title: title.clone(), artist: artist.clone(), album_art_url: album_art_url.clone() };
                build_now_playing_embed(&meta, track_id, dj_name)
            }
            Some(CardView::Queued { item }) => build_priority_now_playing_embed(item),
            None => return build_controls_embed(self.account.as_deref(), media_available),
        };
        embed = embed.author(CreateEmbedAuthor::new(if self.paused { "Paused" } else { "Now Playing" }));
        if let Some(started_at) = self.started_at { embed = embed.timestamp(started_at); }
        embed
    }

    fn buttons(&self) -> Vec<CreateActionRow> {
        build_controls_buttons(self.view.is_some(), self.paused)
    }
}

/// Spawns the only public-card owner. Call once from ready()'s one-shot guard.
pub fn spawn(ctx: Context, text_channel_id: ChannelId, media_available: bool) -> mpsc::UnboundedSender<UiMsg> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run(ctx, text_channel_id, media_available, rx));
    tx
}

async fn sync_card(ctx: &Context, channel: ChannelId, id: &mut Option<MessageId>, state: &CardState, media_available: bool, repost: bool) {
    let embed = state.embed(media_available);
    let components = state.buttons();
    if !repost {
        if let Some(current) = *id {
            match channel.edit_message(ctx, current, EditMessage::new().embeds(vec![embed.clone()]).components(components.clone())).await {
                Ok(_) => return,
                Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)))
                    if response.error.code == 10008 => { *id = None; }
                Err(error) => {
                    // A transient/permission error is not proof of deletion;
                    // preserve the known id and retry rather than duplicating it.
                    tracing::warn!(error = ?error, "failed to refresh music card");
                    return;
                }
            }
        }
    }
    match channel.send_message(ctx, CreateMessage::new().embed(embed).components(components)).await {
        Ok(message) => {
            // Keep the old usable card until its replacement has been posted.
            if let Some(previous) = id.replace(message.id) {
                let _ = channel.delete_message(ctx, previous).await;
            }
            tracing::debug!("posted music card");
        }
        Err(error) => tracing::warn!(error = ?error, "failed to post music card"),
    }
}

async fn run(ctx: Context, channel: ChannelId, media_available: bool, mut rx: mpsc::UnboundedReceiver<UiMsg>) {
    startup_sweep(&ctx, channel).await;
    let mut state = CardState::default();
    let mut card_id = None;
    sync_card(&ctx, channel, &mut card_id, &state, media_available, true).await;
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(30));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh.tick().await;
    loop {
        let message = tokio::select! {
            message = rx.recv() => match message { Some(message) => message, None => break },
            _ = refresh.tick() => {
                sync_card(&ctx, channel, &mut card_id, &state, media_available, false).await;
                continue;
            }
        };
        let repost = matches!(&message, UiMsg::NowPlaying(_) | UiMsg::Idle { .. });
        let history_only = matches!(&message, UiMsg::History(_));
        if let Some(history) = state.apply(message) {
            let embed = match history {
                HistoryView::Spotify { title, artist, track_id, album_art_url, dj_name } => {
                    let meta = SpotifyTrackInfo { title, artist, album_art_url };
                    build_history_embed(&meta, &track_id, &dj_name)
                }
                HistoryView::Queued { item } => build_priority_history_embed(&item),
            };
            let _ = channel.send_message(&ctx, CreateMessage::new().embed(embed)).await;
        }
        if !history_only {
            sync_card(&ctx, channel, &mut card_id, &state, media_available, repost).await;
        }
    }
}
/// Deletes any of the bot's own leftover control/now-playing messages
/// (identified by author plus music buttons or a "🎛️" embed title, since a
/// restart has no other way to recognize its own old cards) before the
/// task starts serving `UiMsg`s.
async fn startup_sweep(ctx: &Context, text_channel_id: ChannelId) {
    use serenity::all::GetMessages;
    let bot_id = ctx.cache.current_user().id;
    let builder = GetMessages::new().limit(20);
    if let Ok(messages) = text_channel_id.messages(ctx, builder).await {
        for msg in &messages {
            if msg.author.id != bot_id {
                continue;
            }
            // Match music controls specifically, leaving other feature
            // messages alone, or recognize one of our legacy control cards
            // (idle "🎛️ Spotibot" or an active "🎛️ {name}"). Matching on the
            // buttons catches the merged now-playing card too, whose title is
            // the track name rather than a "🎛️" string.
            let has_buttons = msg.components.iter().flat_map(|row| &row.components).any(|component| {
                matches!(component,
                    serenity::all::ActionRowComponent::Button(button)
                    if matches!(&button.data, serenity::all::ButtonKind::NonLink { custom_id, .. }
                        if custom_id.starts_with("ctrl_")))
            });
            let is_control_card = msg
                .embeds
                .iter()
                .any(|e| e.title.as_deref().is_some_and(|t| t.starts_with("🎛️")));
            if has_buttons || is_control_card {
                let _ = text_channel_id.delete_message(ctx, msg.id).await;
            }
        }
    }
}

// --- Embed builders ---

fn build_now_playing_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(0x1DB954u32)
        .title(clipped(&meta.title, 256))
        .description(clipped(&meta.artist, 1024))
        .field("Source", "Spotify", true)
        .url(format!("https://open.spotify.com/track/{}", track_id));

    if !spotify_name.is_empty() {
        embed = embed.field("DJ", clipped(spotify_name, 128), true);
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.image(art_url);
    }

    embed
}

fn build_history_embed(meta: &SpotifyTrackInfo, track_id: &str, spotify_name: &str) -> CreateEmbed {
    let footer_text = if spotify_name.is_empty() {
        String::new()
    } else {
        format!("played by {}", spotify_name)
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(format!(
            "[{} — {}](https://open.spotify.com/track/{})",
            clipped(&meta.title, 200), clipped(&meta.artist, 100), track_id
        ));

    if !footer_text.is_empty() {
        embed = embed.footer(CreateEmbedFooter::new(footer_text));
    }

    if let Some(ref art_url) = meta.album_art_url {
        embed = embed.thumbnail(art_url);
    }

    embed
}

fn build_priority_now_playing_embed(item: &QueueItem) -> CreateEmbed {
    let color = item.source.embed_color();
    let title = item.source.display_title();
    let subtitle = item.source.display_subtitle();
    let footer_icon = match &item.source {
        MediaSource::YouTube { .. } => "🎬",
        MediaSource::File { .. } => "📎",
        MediaSource::Spotify { .. } => "🎵",
    };

    let footer_text = match item.source.display_duration() {
        Some(d) => format!("{} {} · {}", footer_icon, item.queued_by, d),
        None => format!("{} {}", footer_icon, item.queued_by),
    };
    let mut embed = CreateEmbed::new()
        .color(color)
        .author(CreateEmbedAuthor::new("Now Playing"))
        .title(clipped(title, 256))
        .description(clipped(&subtitle, 1024))
        .footer(CreateEmbedFooter::new(clipped(&footer_text, 256)));

    if let MediaSource::YouTube { url, thumbnail_url, .. } = &item.source {
        let soundcloud = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|host| host == "soundcloud.com" || host.ends_with(".soundcloud.com"));
        embed = embed.field("Source", if soundcloud { "SoundCloud" } else { "YouTube" }, true);
        if soundcloud { embed = embed.color(0xFF5500u32); }
        embed = embed.url(url);
        if let Some(thumb) = thumbnail_url {
            embed = embed.image(thumb);
        }
    } else if let MediaSource::Spotify { uri, album_art_url, .. } = &item.source {
        embed = embed.field("Source", "Spotify", true);
        if let Some(track_id) = uri.to_uri().strip_prefix("spotify:track:") {
            embed = embed.url(format!("https://open.spotify.com/track/{track_id}"));
        }
        if let Some(art) = album_art_url { embed = embed.image(art); }
    } else {
        embed = embed.field("Source", "Audio file", true);
    }

    embed
}

fn build_priority_history_embed(item: &QueueItem) -> CreateEmbed {
    let footer_text = match item.source.display_duration() {
        Some(d) => format!("played by {} · {}", item.queued_by, d),
        None => format!("played by {}", item.queued_by),
    };
    let description = match &item.source {
        MediaSource::YouTube { title, channel, url, .. } => {
            format!("[{} — {}]({})", clipped(title, 200), clipped(channel, 100), url)
        }
        MediaSource::File { filename, .. } => {
            format!("📎 {}", clipped(filename, 256))
        }
        MediaSource::Spotify { title, artist, .. } => {
            format!("🎵 {} — {}", clipped(title, 200), clipped(artist, 100))
        }
    };

    let mut embed = CreateEmbed::new()
        .color(0x2B2D31u32)
        .description(description)
        .footer(CreateEmbedFooter::new(footer_text));

    if let MediaSource::YouTube { thumbnail_url: Some(thumb), .. } = &item.source {
        embed = embed.thumbnail(thumb);
    } else if let MediaSource::Spotify { album_art_url: Some(art), .. } = &item.source {
        embed = embed.thumbnail(art);
    }

    embed
}

/// Keep text inside Discord's limits, including astral Unicode characters.
pub(super) fn clipped(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max { return text.to_string(); }
    if max == 0 { return String::new(); }
    let mut used = 0;
    let mut result: String = text.chars().take_while(|c| {
        used += c.len_utf16();
        used < max
    }).collect();
    result.push('…');
    result
}

fn build_controls_embed(active_user: Option<&str>, media_available: bool) -> CreateEmbed {
    let description = if media_available {
        "Use **Add music** to search YouTube or paste a Spotify, YouTube or SoundCloud track link.\nSpotify playback needs `/login`."
    } else {
        "Use `/login` to connect Spotify, then **Add music** to paste a track link."
    };
    let mut embed = CreateEmbed::new().color(0x5865F2u32)
        .title("🎛️ Spotibot").description(description);
    if let Some(name) = active_user {
        embed = embed.field("Spotify DJ", clipped(name, 128), true)
            .footer(CreateEmbedFooter::new("Play in Spotify, add a track, or resume the queue"));
    }
    embed
}

fn build_controls_buttons(playing: bool, paused: bool) -> Vec<CreateActionRow> {
    let transport = if playing {
        vec![
            CreateButton::new("ctrl_prev").label("⏮ Previous").style(ButtonStyle::Secondary),
            CreateButton::new("ctrl_pause_toggle").label(if paused { "▶ Resume" } else { "⏸ Pause" }).style(ButtonStyle::Primary),
            CreateButton::new("ctrl_next").label("⏭ Skip").style(ButtonStyle::Secondary),
            CreateButton::new("ctrl_stop").label("⏹ Stop").style(ButtonStyle::Danger),
        ]
    } else {
        vec![CreateButton::new("ctrl_play").label("▶ Play").style(ButtonStyle::Secondary)]
    };
    vec![
        CreateActionRow::Buttons(transport),
        CreateActionRow::Buttons(vec![
            CreateButton::new("ctrl_add_music").label("➕ Add music").style(ButtonStyle::Primary),
            CreateButton::new("ctrl_queue_hint").label("☰ Queue").style(ButtonStyle::Secondary),
            CreateButton::new("ctrl_history").label("🕘 History").style(ButtonStyle::Secondary),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spotify() -> CardView {
        CardView::Spotify { title: "A track".into(), artist: "An artist".into(),
            track_id: "track".into(), album_art_url: None, dj_name: "First DJ".into() }
    }

    #[test]
    fn account_changes_preserve_paused_rendering_and_track_start_time() {
        let mut state = CardState::default();
        state.apply(UiMsg::NowPlaying(spotify()));
        let started = state.started_at;
        state.apply(UiMsg::Buttons { paused: true });
        state.apply(UiMsg::AccountChanged(Some("Second DJ".into())));
        let embed = serde_json::to_value(state.embed(true)).unwrap();
        let buttons = serde_json::to_value(state.buttons()).unwrap();
        assert_eq!(embed["author"]["name"], "Paused");
        assert_eq!(embed["fields"][1]["value"], "Second DJ");
        assert_eq!(buttons[0]["components"][1]["label"], "▶ Resume");
        assert_eq!(state.started_at, started);
    }

    #[test]
    fn rerendering_does_not_emit_history_and_only_track_changes_advance_it() {
        let mut state = CardState::default();
        assert!(state.apply(UiMsg::NowPlaying(spotify())).is_none());
        state.apply(UiMsg::Buttons { paused: true });
        let first = serde_json::to_value(state.embed(true)).unwrap();
        assert_eq!(first, serde_json::to_value(state.embed(true)).unwrap());
        assert!(state.apply(UiMsg::AccountChanged(None)).is_none());
        assert!(matches!(state.apply(UiMsg::NowPlaying(spotify())), Some(HistoryView::Spotify { .. })));
        assert!(!state.paused);
    }

    #[test]
    fn idle_media_controls_are_available_without_a_spotify_account() {
        let state = CardState::default();
        let buttons = serde_json::to_value(state.buttons()).unwrap();
        assert_eq!(buttons[0]["components"][0]["custom_id"], "ctrl_play");
        assert_eq!(buttons[1]["components"][0]["custom_id"], "ctrl_add_music");
        assert_eq!(buttons[1]["components"][1]["custom_id"], "ctrl_queue_hint");
        let embed = serde_json::to_value(state.embed(true)).unwrap();
        assert!(embed["description"].as_str().unwrap().contains("search YouTube"));
        let spotify_only = serde_json::to_value(state.embed(false)).unwrap();
        assert!(!spotify_only["description"].as_str().unwrap().contains("search YouTube"));
    }

    #[test]
    fn soundcloud_cards_link_to_the_actual_track() {
        let item = QueueItem::new(MediaSource::YouTube { url: "https://soundcloud.com/artist/track".into(),
            video_id: "123".into(), title: "Track".into(), channel: "Artist".into(),
            thumbnail_url: None, duration_secs: 90 }, "Listener".into(), 1);
        let embed = serde_json::to_value(build_priority_now_playing_embed(&item)).unwrap();
        assert_eq!(embed["url"], "https://soundcloud.com/artist/track");
        assert_eq!(embed["fields"][0]["value"], "SoundCloud");
        let history = serde_json::to_value(build_priority_history_embed(&item)).unwrap();
        assert!(history["description"].as_str().unwrap().contains("https://soundcloud.com/artist/track"));
    }

    #[test]
    fn long_unicode_metadata_stays_inside_the_embed_title_limit() {
        let meta = SpotifyTrackInfo { title: "🎵".repeat(300), artist: "Artist".into(), album_art_url: None };
        let embed = serde_json::to_value(build_now_playing_embed(&meta, "track", "DJ")).unwrap();
        assert!(embed["title"].as_str().unwrap().encode_utf16().count() <= 256);
        assert_eq!(clipped("🎵🎵", 3), "🎵…");
    }
}
