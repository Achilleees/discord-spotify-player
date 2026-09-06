//! Private discovery menus. Only a validated, single-use selection reaches
//! the same track request path used by slash commands.

use super::{bot::Handler, commands::PLAY_VOICE_REQUIRED, ui::clipped};
use crate::youtube::metadata::{search_youtube, YoutubeSearchResult, SEARCH_RESULTS};
use serenity::all::{
    ActionRowComponent, ButtonStyle, ComponentInteraction, Context, CreateActionRow,
    CreateAllowedMentions, CreateButton, CreateEmbed, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, EditInteractionResponse, GuildId,
    InputTextStyle, ModalInteraction, UserId,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MODAL_ID: &str = "music_add_modal";
const MENU_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_MENUS: usize = 64;

struct SearchMenu {
    user: UserId,
    guild: GuildId,
    expires: Instant,
    results: Vec<YoutubeSearchResult>,
}

#[derive(Default)]
pub(super) struct SearchMenus {
    entries: HashMap<Uuid, SearchMenu>,
}

impl SearchMenus {
    fn insert(
        &mut self,
        user: UserId,
        guild: GuildId,
        mut results: Vec<YoutubeSearchResult>,
        now: Instant,
    ) -> Uuid {
        self.entries.retain(|_, entry| now < entry.expires);
        if self.entries.len() >= MAX_MENUS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(id, _)| *id)
            {
                self.entries.remove(&oldest);
            }
        }
        results.truncate(SEARCH_RESULTS);
        let id = Uuid::new_v4();
        self.entries.insert(
            id,
            SearchMenu {
                user,
                guild,
                expires: now + MENU_TTL,
                results,
            },
        );
        id
    }

    fn take(
        &mut self,
        id: Uuid,
        index: usize,
        user: UserId,
        guild: GuildId,
        now: Instant,
    ) -> Option<YoutubeSearchResult> {
        self.entries.retain(|_, entry| now < entry.expires);
        let entry = self.entries.get(&id)?;
        if entry.user != user || entry.guild != guild || index >= entry.results.len() {
            return None;
        }
        Some(self.entries.remove(&id)?.results.swap_remove(index))
    }
}

#[derive(Debug, PartialEq)]
enum MusicInput {
    Link(String),
    Search(String),
}

enum MusicReply {
    Text(String),
    Choices(Vec<YoutubeSearchResult>),
}

fn music_input(raw: &str) -> Option<MusicInput> {
    let input = raw.trim();
    if input.is_empty() || input.chars().count() > 500 || input.chars().any(char::is_control) {
        return None;
    }
    let lower = input.to_ascii_lowercase();
    let link = lower.contains("://")
        || lower.starts_with("spotify:")
        || [
            "www.",
            "open.spotify.com/",
            "youtube.com/",
            "music.youtube.com/",
            "youtu.be/",
            "soundcloud.com/",
            "on.soundcloud.com/",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    Some(if link {
        MusicInput::Link(input.into())
    } else {
        MusicInput::Search(input.into())
    })
}

fn selection(custom_id: &str) -> Option<(Uuid, usize)> {
    let mut parts = custom_id.split(':');
    if parts.next()? != "music_pick" {
        return None;
    }
    let id = Uuid::parse_str(parts.next()?).ok()?;
    let index = parts.next()?.parse::<usize>().ok()?;
    (parts.next().is_none() && index < SEARCH_RESULTS).then_some((id, index))
}

fn result_message(id: Uuid, results: &[YoutubeSearchResult]) -> EditInteractionResponse {
    let mut description = String::new();
    for (index, result) in results.iter().enumerate() {
        let duration = result
            .duration_secs
            .map(|s| format!(" · {}:{:02}", s / 60, s % 60))
            .unwrap_or_default();
        description.push_str(&format!(
            "**{}. {}**\n{}{}\n\n",
            index + 1,
            clipped(&result.title.replace(['\n', '\r'], " "), 120),
            clipped(&result.channel.replace(['\n', '\r'], " "), 60),
            duration
        ));
    }
    let buttons = results
        .iter()
        .enumerate()
        .map(|(index, _)| {
            CreateButton::new(format!("music_pick:{id}:{index}"))
                .label((index + 1).to_string())
                .style(ButtonStyle::Primary)
        })
        .collect();
    EditInteractionResponse::new()
        .content("")
        .embed(
            CreateEmbed::new()
                .title("Choose a track")
                .color(0xFF0000u32)
                .description(description)
                .footer(serenity::all::CreateEmbedFooter::new(
                    "Adds to the queue · starts if idle · choices expire in 5 minutes",
                )),
        )
        .components(vec![CreateActionRow::Buttons(buttons)])
        .allowed_mentions(CreateAllowedMentions::new())
}

async fn reply(ctx: &Context, component: &ComponentInteraction, text: &str) {
    let _ = component
        .create_response(
            ctx,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(text)
                    .ephemeral(true),
            ),
        )
        .await;
}

impl Handler {
    pub(super) async fn open_music_modal(&self, ctx: &Context, component: &ComponentInteraction) {
        if !self.user_can_play(ctx, component.user.id) {
            reply(ctx, component, PLAY_VOICE_REQUIRED).await;
            return;
        }
        let placeholder = if self.ytdlp_available {
            "Song, artist, or Spotify / YouTube / SoundCloud link"
        } else {
            "Spotify track link"
        };
        let modal =
            CreateModal::new(MODAL_ID, "Add music").components(vec![CreateActionRow::InputText(
                CreateInputText::new(
                    InputTextStyle::Short,
                    "What would you like to play?",
                    "music_query",
                )
                .placeholder(placeholder)
                .max_length(500)
                .required(true),
            )]);
        let _ = component
            .create_response(ctx, CreateInteractionResponse::Modal(modal))
            .await;
    }

    pub(super) async fn handle_music_modal(&self, ctx: &Context, modal: &ModalInteraction) {
        if modal.data.custom_id != MODAL_ID || modal.guild_id != Some(self.guild_id) {
            return;
        }
        if modal.defer_ephemeral(ctx).await.is_err() {
            return;
        }
        let raw = modal
            .data
            .components
            .iter()
            .flat_map(|row| &row.components)
            .find_map(|component| match component {
                ActionRowComponent::InputText(input) if input.custom_id == "music_query" => {
                    input.value.as_deref()
                }
                _ => None,
            });
        let result = if !self.user_can_play(ctx, modal.user.id) {
            MusicReply::Text(PLAY_VOICE_REQUIRED.to_string())
        } else if self.media_lookup_on_cooldown(modal.user.id) {
            MusicReply::Text("Try again in a few seconds.".into())
        } else {
            match raw.and_then(music_input) {
                None => MusicReply::Text("Enter a song, artist or supported track link.".into()),
                Some(MusicInput::Link(url)) => {
                    MusicReply::Text(self.play_link(ctx, &modal.user, url).await)
                }
                Some(MusicInput::Search(_)) if !self.ytdlp_available => MusicReply::Text(
                    "Text search is unavailable on this bot. Paste a Spotify track link instead."
                        .into(),
                ),
                Some(MusicInput::Search(query)) => match search_youtube(&query).await {
                    Ok(results) => MusicReply::Choices(results),
                    Err(error) => MusicReply::Text(error.to_string()),
                },
            }
        };
        let response = match result {
            MusicReply::Choices(results) if !results.is_empty() => {
                let id = self.search_menus.lock().insert(
                    modal.user.id,
                    self.guild_id,
                    results.clone(),
                    Instant::now(),
                );
                result_message(id, &results)
            }
            MusicReply::Choices(_) => EditInteractionResponse::new()
                .content("No playable results found. Try another song or artist."),
            MusicReply::Text(text) => EditInteractionResponse::new().content(clipped(&text, 1900)),
        };
        let _ = modal
            .edit_response(ctx, response.allowed_mentions(CreateAllowedMentions::new()))
            .await;
    }

    pub(super) async fn handle_music_pick(&self, ctx: &Context, component: &ComponentInteraction) {
        if !self.user_can_play(ctx, component.user.id) {
            reply(ctx, component, PLAY_VOICE_REQUIRED).await;
            return;
        }
        // Claim synchronously before any await: only one concurrent click can
        // consume a menu. Invalid owners/indices never consume another menu.
        let picked = selection(&component.data.custom_id).and_then(|(id, index)| {
            self.search_menus.lock().take(
                id,
                index,
                component.user.id,
                self.guild_id,
                Instant::now(),
            )
        });
        let Some(picked) = picked else {
            reply(
                ctx,
                component,
                "These choices expired or were already used. Open Add music again.",
            )
            .await;
            return;
        };
        if component.defer(ctx).await.is_err() {
            return;
        }
        let text = self.play_link(ctx, &component.user, picked.url).await;
        let _ = component
            .edit_response(
                ctx,
                EditInteractionResponse::new()
                    .content(clipped(&text, 1900))
                    .embeds(vec![])
                    .components(vec![])
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results() -> Vec<YoutubeSearchResult> {
        vec![YoutubeSearchResult {
            url: "https://www.youtube.com/watch?v=aaaaaaaaaaa".into(),
            title: "A track".into(),
            channel: "An artist".into(),
            duration_secs: Some(90),
        }]
    }

    #[test]
    fn a_result_menu_can_be_consumed_only_once() {
        let now = Instant::now();
        let mut menus = SearchMenus::default();
        let id = menus.insert(UserId::new(1), GuildId::new(2), results(), now);
        assert!(menus
            .take(id, 0, UserId::new(1), GuildId::new(2), now)
            .is_some());
        assert!(menus
            .take(id, 0, UserId::new(1), GuildId::new(2), now)
            .is_none());
    }

    #[test]
    fn wrong_owner_guild_or_index_cannot_destroy_the_valid_choice() {
        let now = Instant::now();
        let mut menus = SearchMenus::default();
        let id = menus.insert(UserId::new(1), GuildId::new(2), results(), now);
        assert!(menus
            .take(id, 0, UserId::new(9), GuildId::new(2), now)
            .is_none());
        assert!(menus
            .take(id, 0, UserId::new(1), GuildId::new(9), now)
            .is_none());
        assert!(menus
            .take(id, 5, UserId::new(1), GuildId::new(2), now)
            .is_none());
        assert!(menus
            .take(id, 0, UserId::new(1), GuildId::new(2), now)
            .is_some());
    }

    #[test]
    fn expiry_is_enforced_at_selection_and_storage_stays_bounded() {
        let now = Instant::now();
        let mut menus = SearchMenus::default();
        let expired = menus.insert(UserId::new(1), GuildId::new(2), results(), now);
        assert!(menus
            .take(expired, 0, UserId::new(1), GuildId::new(2), now + MENU_TTL)
            .is_none());
        let oldest = menus.insert(UserId::new(1), GuildId::new(2), results(), now);
        for i in 1..=MAX_MENUS {
            menus.insert(
                UserId::new(1),
                GuildId::new(2),
                results(),
                now + Duration::from_secs(i as u64),
            );
        }
        assert_eq!(menus.entries.len(), MAX_MENUS);
        assert!(!menus.entries.contains_key(&oldest));
    }

    #[test]
    fn song_names_and_links_take_distinct_paths() {
        assert_eq!(
            music_input(" AC/DC live "),
            Some(MusicInput::Search("AC/DC live".into()))
        );
        for link in [
            "spotify:track:abc",
            "youtube.com/watch?v=abc",
            "https://127.0.0.1/private",
            "file:///tmp/song",
        ] {
            assert!(
                matches!(music_input(link), Some(MusicInput::Link(_))),
                "{link}"
            );
        }
        assert!(music_input("\t").is_none());
        assert!(music_input("two\nlines").is_none());
    }

    #[test]
    fn forged_button_suffixes_are_rejected() {
        let id = Uuid::new_v4();
        assert_eq!(selection(&format!("music_pick:{id}:0")), Some((id, 0)));
        for bad in [
            format!("music_pick:{id}:5"),
            format!("music_pick:{id}:-1"),
            format!("music_pick:{id}:0:extra"),
            "music_pick:not-a-uuid:0".into(),
        ] {
            assert!(selection(&bad).is_none());
        }
    }

    #[test]
    fn five_long_choices_fit_one_button_row_and_the_embed_limit() {
        let mut long = results().remove(0);
        long.title = "🎵".repeat(240);
        long.channel = "Artist".repeat(80);
        let data = serde_json::to_value(result_message(Uuid::new_v4(), &vec![long; 5])).unwrap();
        assert!(
            data["embeds"][0]["description"]
                .as_str()
                .unwrap()
                .encode_utf16()
                .count()
                < 4096
        );
        let buttons = data["components"][0]["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 5);
        assert!(buttons
            .iter()
            .all(|b| b["custom_id"].as_str().unwrap().len() < 100));
    }
}
