//! Nob's private clip picker. Each panel has one opaque, single-use token;
//! paging keeps its original room/revision, while Refresh explicitly renews it.

use super::{bot::Handler, ui::clipped};
use crate::{routing::VoiceActivity, runtime::Profile};
use serenity::all::*;
use std::{
    collections::HashMap,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

const PREFIX: &str = "sboard:";
const TTL: Duration = Duration::from_secs(300);
const MAX_MENUS: usize = 64;
const PAGE_SIZE: usize = 10;

fn unavailable(
    owned_room: Option<u64>,
    activity: Option<VoiceActivity>,
    bot_room: Option<u64>,
    user_room: Option<u64>,
    clip_busy: bool,
) -> bool {
    if clip_busy {
        return true;
    }
    match (owned_room, activity, bot_room) {
        (None, None, None) => false,
        (Some(room), Some(VoiceActivity::Music), Some(bot_room)) => {
            user_room != Some(room) || bot_room != room
        }
        _ => true,
    }
}

#[derive(Clone)]
struct Choice {
    id: String,
    label: String,
}

#[derive(Clone)]
struct Menu {
    user: u64,
    guild: u64,
    room: Option<u64>,
    generation: u64,
    busy: bool,
    expires: Instant,
    page: usize,
    choices: Arc<[Choice]>,
}

impl Menu {
    fn pages(&self) -> usize {
        self.choices.len().div_ceil(PAGE_SIZE).max(1)
    }

    fn bounds(&self) -> Range<usize> {
        let start = self.page.min(self.pages() - 1) * PAGE_SIZE;
        start..(start + PAGE_SIZE).min(self.choices.len())
    }

    fn turn_page(&mut self, forward: bool) {
        self.page = self.page.min(self.pages() - 1);
        self.page = if forward {
            (self.page + 1).min(self.pages() - 1)
        } else {
            self.page.saturating_sub(1)
        };
    }

    fn selection(
        &self,
        slot: usize,
        current_room: Option<u64>,
        current_generation: u64,
    ) -> Result<(&Choice, u64), &'static str> {
        let room = self
            .room
            .filter(|room| *room != 0)
            .ok_or("Join a voice call, then press Refresh to choose a sound.")?;
        if current_room != Some(room) {
            return Err("Your voice room changed. Press Refresh before choosing a sound.");
        }
        if current_generation != self.generation {
            return Err("nob's voice activity changed. Press Refresh before choosing a sound.");
        }
        if self.busy {
            return Err(
                "Join nob's room and wait for any current sound to finish, then press Refresh.",
            );
        }
        let choice = self.choices[self.bounds()]
            .get(slot)
            .ok_or("That sound is not on this page. Press Refresh and choose again.")?;
        Ok((choice, room))
    }
}

#[derive(Default)]
pub(super) struct Menus {
    entries: HashMap<String, Menu>,
}

impl Menus {
    fn insert(&mut self, mut menu: Menu) -> String {
        let now = Instant::now();
        self.entries.retain(|_, menu| menu.expires > now);
        if self.entries.len() >= MAX_MENUS {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, menu)| menu.expires)
                .map(|(id, _)| id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        menu.expires = now + TTL;
        let id = uuid::Uuid::new_v4().to_string();
        self.entries.insert(id.clone(), menu);
        id
    }

    fn take(&mut self, id: &str, user: u64, guild: u64) -> Option<Menu> {
        if self.entries.get(id).is_none_or(|menu| {
            menu.user != user || menu.guild != guild || menu.expires <= Instant::now()
        }) {
            return None;
        }
        self.entries.remove(id)
    }
}

pub(super) fn register_commands() -> Vec<CreateCommand> {
    vec![CreateCommand::new("soundboard")
        .description("Play a sound over nob's music or invite him for a quick visit")]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Previous,
    Next,
    Refresh,
    Close,
    Pick(usize),
}

fn parse_action(custom_id: &str) -> Option<(&str, Action)> {
    let (id, action) = custom_id.strip_prefix(PREFIX)?.split_once(':')?;
    if id.is_empty() {
        return None;
    }
    let action = match action {
        "previous" => Action::Previous,
        "next" => Action::Next,
        "refresh" => Action::Refresh,
        "close" => Action::Close,
        value => Action::Pick(
            value
                .strip_prefix("pick")?
                .parse::<usize>()
                .ok()
                .filter(|slot| *slot < PAGE_SIZE)?,
        ),
    };
    Some((id, action))
}

fn button(id: &str, action: &str, label: &str) -> CreateButton {
    CreateButton::new(format!("{PREFIX}{id}:{action}"))
        .label(clipped(label, 80))
        .style(ButtonStyle::Secondary)
}

fn panel(id: &str, menu: &Menu, notice: &str) -> EditInteractionResponse {
    let visible = &menu.choices[menu.bounds()];
    let mut rows: Vec<_> = visible
        .chunks(5)
        .enumerate()
        .map(|(row, choices)| {
            CreateActionRow::Buttons(
                choices
                    .iter()
                    .enumerate()
                    .map(|(column, choice)| {
                        button(id, &format!("pick{}", row * 5 + column), &choice.label)
                            .disabled(menu.room.is_none() || menu.busy)
                    })
                    .collect(),
            )
        })
        .collect();
    let page = menu.page.min(menu.pages() - 1);
    rows.push(CreateActionRow::Buttons(vec![
        button(id, "previous", "Previous").disabled(page == 0),
        button(id, "next", "Next").disabled(page + 1 == menu.pages()),
        button(id, "refresh", "Refresh"),
        button(id, "close", "Close"),
    ]));
    let heading = if menu.choices.is_empty() {
        "**nob's soundboard**\nNo sounds are available yet.".into()
    } else {
        format!(
            "**nob's soundboard** · Page {} / {}\nPlay a sound over nob's music in your room. When he's free, he'll visit, play it and leave.",
            page + 1,
            menu.pages(),
        )
    };
    let availability = match (menu.choices.is_empty(), menu.busy, menu.room) {
        (true, _, _) => String::new(),
        (false, true, _) => {
            "\nJoin nob's room and wait for any current sound to finish, then press Refresh.".into()
        }
        (false, false, Some(room)) => format!("\nYour voice room: <#{room}>."),
        (false, false, None) => "\nJoin a voice call, then press Refresh to choose a sound.".into(),
    };
    // Keep the panel's navigation help visible even when a playback outcome is
    // long. Discord limits content and labels in UTF-16 code units.
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", clipped(notice, 1400))
    };
    EditInteractionResponse::new()
        .content(clipped(&format!("{heading}{availability}{notice}"), 1900))
        .components(rows)
        .allowed_mentions(CreateAllowedMentions::new())
}

enum Response<'a> {
    Command(&'a CommandInteraction),
    Component(&'a ComponentInteraction),
}

impl Response<'_> {
    async fn defer(&self, ctx: &Context) -> bool {
        match self {
            Self::Command(interaction) => interaction.defer_ephemeral(ctx).await,
            Self::Component(interaction) => interaction.defer(ctx).await,
        }
        .is_ok()
    }

    async fn edit(&self, ctx: &Context, response: EditInteractionResponse) {
        let response = response.allowed_mentions(CreateAllowedMentions::new());
        let result = match self {
            Self::Command(interaction) => interaction.edit_response(ctx, response).await,
            Self::Component(interaction) => interaction.edit_response(ctx, response).await,
        };
        if let Err(error) = result {
            tracing::debug!(%error, "soundboard response update failed");
        }
    }

    async fn text(&self, ctx: &Context, text: &str) {
        self.edit(
            ctx,
            EditInteractionResponse::new()
                .content(clipped(text, 1900))
                .components(vec![]),
        )
        .await;
    }
}

impl Handler {
    fn soundboard_menu(&self, ctx: &Context, user: UserId) -> Menu {
        let (generation, owned_room, activity) = self.voice_owner.status();
        let (bot_room, user_room) = self.voice_channels(ctx, user);
        Menu {
            user: user.get(),
            guild: self.guild_id.get(),
            room: user_room.map(|channel| channel.get()),
            generation,
            busy: unavailable(
                owned_room,
                activity,
                bot_room.map(|room| room.get()),
                user_room.map(|room| room.get()),
                self.voice_owner.overlay_busy() || self.bridge.has_overlay_audio(),
            ),
            expires: Instant::now() + TTL,
            page: 0,
            choices: self
                .soundboard
                .clips()
                .iter()
                .map(|clip| Choice {
                    id: clip.id.clone(),
                    label: clip.label.clone(),
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }

    fn refresh_soundboard_menu(&self, ctx: &Context, menu: &mut Menu) {
        let (generation, owned_room, activity) = self.voice_owner.status();
        let (bot_room, user_room) = self.voice_channels(ctx, UserId::new(menu.user));
        menu.room = user_room.map(|channel| channel.get());
        menu.generation = generation;
        menu.busy = unavailable(
            owned_room,
            activity,
            bot_room.map(|room| room.get()),
            menu.room,
            self.voice_owner.overlay_busy() || self.bridge.has_overlay_audio(),
        );
    }

    async fn render_soundboard(
        &self,
        ctx: &Context,
        response: &Response<'_>,
        menu: Menu,
        notice: &str,
    ) {
        let id = self.soundboard_menus.lock().insert(menu.clone());
        response.edit(ctx, panel(&id, &menu, notice)).await;
    }

    pub(super) async fn dispatch_soundboard(
        &self,
        ctx: &Context,
        interaction: &Interaction,
    ) -> bool {
        if self.config.profile != Profile::Nob {
            return false;
        }
        match interaction {
            Interaction::Command(command) if command.data.name == "soundboard" => {
                let response = Response::Command(command);
                if !response.defer(ctx).await {
                    return true;
                }
                if command.guild_id != Some(self.guild_id) {
                    response
                        .text(ctx, "Open /soundboard in nob's server.")
                        .await;
                    return true;
                }
                self.render_soundboard(
                    ctx,
                    &response,
                    self.soundboard_menu(ctx, command.user.id),
                    "",
                )
                .await;
                true
            }
            Interaction::Component(component) if component.data.custom_id.starts_with(PREFIX) => {
                let parsed = parse_action(&component.data.custom_id);
                let accepted = parsed.and_then(|(id, action)| {
                    (component.guild_id == Some(self.guild_id))
                        .then(|| {
                            self.soundboard_menus.lock().take(
                                id,
                                component.user.id.get(),
                                self.guild_id.get(),
                            )
                        })
                        .flatten()
                        .map(|menu| (menu, action))
                });
                let Some((mut menu, action)) = accepted else {
                    // A duplicate can arrive while the accepted click is still
                    // playing. Never overwrite that click's panel or outcome.
                    let _ = component
                        .create_response(
                            ctx,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .content("This menu expired or was already used. Open /soundboard again.")
                                    .ephemeral(true)
                                    .allowed_mentions(CreateAllowedMentions::new()),
                            ),
                        )
                        .await;
                    return true;
                };
                let response = Response::Component(component);
                if !response.defer(ctx).await {
                    return true;
                }
                match action {
                    Action::Close => {
                        response.text(ctx, "Soundboard closed.").await;
                        return true;
                    }
                    Action::Previous => menu.turn_page(false),
                    Action::Next => menu.turn_page(true),
                    Action::Refresh => self.refresh_soundboard_menu(ctx, &mut menu),
                    Action::Pick(slot) => {
                        let selection = menu.selection(
                            slot,
                            self.voice_channels(ctx, component.user.id)
                                .1
                                .map(|channel| channel.get()),
                            self.voice_owner.snapshot().0,
                        );
                        let (choice, room) = match selection {
                            Ok((choice, room)) => (choice.clone(), room),
                            Err(notice) => {
                                self.render_soundboard(ctx, &response, menu, notice).await;
                                return true;
                            }
                        };
                        response.text(ctx, "Asking nob to play your sound…").await;
                        let outcome = self
                            .play_soundboard_clip(
                                ctx,
                                component.user.id,
                                &choice.id,
                                room,
                                menu.generation,
                            )
                            .await;
                        self.refresh_soundboard_menu(ctx, &mut menu);
                        self.render_soundboard(ctx, &response, menu, &outcome).await;
                        return true;
                    }
                }
                self.render_soundboard(ctx, &response, menu, "").await;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_room_music_accepts_sounds_but_other_rooms_and_clips_do_not() {
        assert!(!unavailable(None, None, None, Some(10), false));
        assert!(!unavailable(None, None, None, None, false));
        assert!(!unavailable(
            Some(10),
            Some(VoiceActivity::Music),
            Some(10),
            Some(10),
            false
        ));
        for (owned, activity, bot, user, busy) in [
            (
                Some(10),
                Some(VoiceActivity::Music),
                Some(10),
                Some(20),
                false,
            ),
            (Some(10), Some(VoiceActivity::Music), Some(10), None, false),
            (Some(10), Some(VoiceActivity::Music), None, Some(10), false),
            (
                Some(10),
                Some(VoiceActivity::Music),
                Some(20),
                Some(10),
                false,
            ),
            (
                Some(10),
                Some(VoiceActivity::Music),
                Some(10),
                Some(10),
                true,
            ),
            (
                Some(10),
                Some(VoiceActivity::Soundboard),
                Some(10),
                Some(10),
                false,
            ),
            (None, None, Some(10), Some(10), false),
        ] {
            assert!(unavailable(owned, activity, bot, user, busy));
        }
    }

    fn menu(count: usize) -> Menu {
        Menu {
            user: 1,
            guild: 2,
            room: Some(3),
            generation: 4,
            busy: false,
            expires: Instant::now() + TTL,
            page: 0,
            choices: (0..count)
                .map(|index| Choice {
                    id: format!("clip-{index}"),
                    label: format!("Sound {index}"),
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }

    #[test]
    fn other_users_and_guilds_cannot_consume_or_reuse_a_menu() {
        let mut menus = Menus::default();
        let id = menus.insert(menu(1));
        assert!(menus.take(&id, 9, 2).is_none());
        assert!(menus.take(&id, 1, 9).is_none());
        assert!(menus.take(&id, 1, 2).is_some());
        assert!(menus.take(&id, 1, 2).is_none());
    }

    #[test]
    fn expiry_and_capacity_bound_retained_panels() {
        let mut menus = Menus::default();
        let expired = menus.insert(menu(1));
        menus.entries.get_mut(&expired).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(menus.take(&expired, 1, 2).is_none());
        menus.insert(menu(1));
        assert!(!menus.entries.contains_key(&expired));
        menus.entries.clear();
        let oldest = menus.insert(menu(1));
        menus.entries.get_mut(&oldest).unwrap().expires = Instant::now() + Duration::from_secs(1);
        for _ in 0..MAX_MENUS {
            menus.insert(menu(1));
        }
        assert_eq!(menus.entries.len(), MAX_MENUS);
        assert!(!menus.entries.contains_key(&oldest));
    }

    #[test]
    fn selection_stays_in_its_room_revision_and_visible_page() {
        let mut menu = menu(21);
        menu.turn_page(true);
        assert_eq!(menu.selection(0, Some(3), 4).unwrap().0.id, "clip-10");
        assert!(menu.selection(10, Some(3), 4).is_err());
        assert!(menu.selection(0, Some(9), 4).is_err());
        assert!(menu.selection(0, None, 4).is_err());
        assert!(menu.selection(0, Some(3), 5).is_err());
        menu.turn_page(true);
        assert_eq!(menu.selection(0, Some(3), 4).unwrap().0.id, "clip-20");
        assert!(menu.selection(1, Some(3), 4).is_err());
        menu.turn_page(true);
        assert_eq!(menu.page, 2);
        for _ in 0..4 {
            menu.turn_page(false);
        }
        assert_eq!(menu.page, 0);
        assert_eq!((menu.room, menu.generation), (Some(3), 4));
        menu.room = None;
        assert!(menu.selection(0, Some(3), 4).is_err());
    }

    #[test]
    fn malformed_and_out_of_page_action_ids_are_rejected() {
        assert_eq!(
            parse_action("sboard:opaque:pick9"),
            Some(("opaque", Action::Pick(9)))
        );
        assert_eq!(
            parse_action("sboard:opaque:refresh"),
            Some(("opaque", Action::Refresh))
        );
        for id in [
            "route:opaque:pick0",
            "sboard::pick0",
            "sboard:opaque",
            "sboard:opaque:pick10",
            "sboard:opaque:pick-1",
            "sboard:opaque:pick99999999999999999999999",
            "sboard:opaque:pick0:extra",
            "sboard:opaque:unknown",
        ] {
            assert_eq!(parse_action(id), None, "{id}");
        }
    }

    #[test]
    fn presentation_keeps_discord_limits_and_exposes_only_visible_choices() {
        let mut menu = menu(23);
        Arc::make_mut(&mut menu.choices)[10].label = "🎉".repeat(100);
        menu.turn_page(true);
        let id = uuid::Uuid::new_v4().to_string();
        let value = serde_json::to_value(panel(&id, &menu, &"🎉".repeat(2000))).unwrap();
        assert!(value["content"].as_str().unwrap().encode_utf16().count() <= 2000);
        let rows = value["components"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let mut clip_buttons = 0;
        for row in rows {
            let buttons = row["components"].as_array().unwrap();
            assert!(buttons.len() <= 5);
            for button in buttons {
                assert!(button["label"].as_str().unwrap().encode_utf16().count() <= 80);
                let custom_id = button["custom_id"].as_str().unwrap();
                assert!(custom_id.len() <= 100);
                assert!(!custom_id.contains("clip-"));
                if let Some((_, Action::Pick(slot))) = parse_action(custom_id) {
                    assert_eq!(
                        menu.selection(slot, Some(3), 4).unwrap().0.id,
                        format!("clip-{}", slot + 10)
                    );
                    clip_buttons += 1;
                }
            }
        }
        assert_eq!(clip_buttons, PAGE_SIZE);
        assert_eq!(value["allowed_mentions"]["parse"], serde_json::json!([]));
    }

    #[test]
    fn empty_and_last_pages_have_no_dead_clip_buttons() {
        let empty = serde_json::to_value(panel("id", &menu(0), "")).unwrap();
        assert!(empty["content"]
            .as_str()
            .unwrap()
            .contains("No sounds are available yet"));
        assert!(!empty["content"]
            .as_str()
            .unwrap()
            .contains("Join a voice call"));
        assert_eq!(empty["components"].as_array().unwrap().len(), 1);
        assert_eq!(empty["components"][0]["components"][0]["disabled"], true);
        assert_eq!(empty["components"][0]["components"][1]["disabled"], true);
        let mut last = menu(21);
        last.page = usize::MAX;
        let value = serde_json::to_value(panel("id", &last, "")).unwrap();
        assert!(value["content"].as_str().unwrap().contains("Page 3 / 3"));
        assert_eq!(
            value["components"][0]["components"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(value["components"][1]["components"][1]["disabled"], true);
        let commands = serde_json::to_value(register_commands()).unwrap();
        assert_eq!(commands.as_array().unwrap().len(), 1);
        assert_eq!(commands[0]["name"], "soundboard");
    }

    #[test]
    fn busy_and_out_of_voice_panels_keep_refresh_while_disabling_sounds() {
        for (busy, room, expected) in [
            (true, Some(3), "wait for any current sound to finish"),
            (false, None, "Join a voice call"),
        ] {
            let mut menu = menu(1);
            menu.busy = busy;
            menu.room = room;
            let value = serde_json::to_value(panel("id", &menu, "")).unwrap();
            assert!(value["content"].as_str().unwrap().contains(expected));
            assert!(menu.selection(0, Some(3), 4).is_err());
            assert_eq!(value["components"][0]["components"][0]["disabled"], true);
            let navigation = &value["components"][1]["components"];
            assert_ne!(navigation[2]["disabled"], true);
            assert_ne!(navigation[3]["disabled"], true);
        }
    }
}
