//! Nob's compact command interface. Private menus bind their owner, guild,
//! room and performer generation; each accepted click consumes its menu token.

use super::{bot::Handler, ui::clipped};
use crate::routing::{
    self,
    transport::{self, Client},
    Action, Attachment, CommandMode, Control, Reply, SearchHit, Status, View,
};
use serenity::all::*;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(300);
const MAX_MENUS: usize = 64;

#[derive(Clone)]
struct Endpoint {
    client: Client,
    name: &'static str,
    status: Option<Status>,
}

#[derive(Clone)]
struct Menu {
    user: u64,
    guild: u64,
    room: Option<u64>,
    expires: Instant,
    endpoints: Vec<Endpoint>,
    selected: Option<usize>,
    pending: Option<Action>,
    hits: Vec<SearchHit>,
    queued: bool,
    next: bool,
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
    let mut commands = vec![
        CreateCommand::new("play")
            .description("Play music in your voice room")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "query",
                    "Song name, Spotify, YouTube or SoundCloud link",
                )
                .max_length(4096),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::Attachment,
                "file",
                "Audio attachment",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::Boolean,
                "next",
                "Play immediately after the current track",
            )),
        CreateCommand::new("music")
            .description("Open your private music controls, queue and Spotify accounts"),
        CreateCommand::new("server").description("Open nob's server tools"),
    ];
    commands.extend(super::soundboard::register_commands());
    commands
}

enum InteractionRef<'a> {
    Command(&'a CommandInteraction),
    Component(&'a ComponentInteraction),
    Modal(&'a ModalInteraction),
}

impl InteractionRef<'_> {
    async fn defer(&self, ctx: &Context) -> bool {
        match self {
            Self::Command(i) => i.defer_ephemeral(ctx).await,
            Self::Component(i) => i.defer(ctx).await,
            Self::Modal(i) => i.defer_ephemeral(ctx).await,
        }
        .is_ok()
    }
    async fn edit(&self, ctx: &Context, response: EditInteractionResponse) {
        let response = response.allowed_mentions(CreateAllowedMentions::new());
        let _ = match self {
            Self::Command(i) => i.edit_response(ctx, response).await,
            Self::Component(i) => i.edit_response(ctx, response).await,
            Self::Modal(i) => i.edit_response(ctx, response).await,
        };
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

fn button(id: &str, action: &str, label: &str) -> CreateButton {
    CreateButton::new(format!("route:{id}:{action}"))
        .label(label)
        .style(ButtonStyle::Secondary)
}

fn panel(id: &str, menu: &Menu, notice: &str) -> EditInteractionResponse {
    let selected = menu.selected.and_then(|i| menu.endpoints.get(i));
    let Some(endpoint) = selected else {
        let choices = menu
            .endpoints
            .iter()
            .enumerate()
            .map(|(index, endpoint)| {
                button(id, &format!("select{index}"), endpoint.name)
                    .disabled(endpoint.status.is_none())
            })
            .collect();
        let rooms = menu
            .endpoints
            .iter()
            .map(|endpoint| {
                let state = endpoint
                    .status
                    .as_ref()
                    .map(|s| {
                        s.target
                            .room
                            .map(|room| format!("in <#{room}> — {}", s.now))
                            .unwrap_or_else(|| "available".into())
                    })
                    .unwrap_or_else(|| "unavailable".into());
                format!("**{}**: {state}", endpoint.name)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return EditInteractionResponse::new()
            .content(clipped(&format!("{notice}\nChoose a bot.\n{rooms}"), 1900))
            .components(vec![CreateActionRow::Buttons(choices)]);
    };
    let status = endpoint
        .status
        .as_ref()
        .expect("only available endpoints are selectable");
    let location = status
        .target
        .room
        .map(|room| format!("<#{room}>"))
        .unwrap_or_else(|| "out of voice".into());
    let capability = if status.media {
        ""
    } else {
        "\nSpotify only; search, YouTube, SoundCloud and files are unavailable."
    };
    let rows = if !menu.hits.is_empty() {
        menu.hits
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                CreateActionRow::Buttons(vec![button(
                    id,
                    &format!("pick{index}"),
                    &clipped(&format!("{}. {}", index + 1, hit.title), 80),
                )])
            })
            .collect()
    } else {
        vec![
            CreateActionRow::Buttons(vec![
                button(id, "play", "Play"),
                button(id, "pause", "Pause / resume"),
                button(id, "previous", "Back"),
                button(id, "skip", "Skip"),
                button(id, "stop", "Stop"),
            ]),
            CreateActionRow::Buttons(vec![
                button(id, "add", "Add music"),
                button(id, "enqueue", "Queue a track"),
                button(id, "queue", "Queue"),
                button(id, "history", "History"),
                button(id, "clear", "Clear queue"),
            ]),
            CreateActionRow::Buttons(vec![
                button(id, "account", "Spotify account"),
                button(id, "login", "Log in"),
                button(id, "logout", "Log out"),
                button(id, "forget", "Forget login"),
            ]),
            CreateActionRow::Buttons(vec![
                button(id, "announce", "Announcements"),
                button(id, "refresh", "Refresh"),
                button(id, "choose", "Choose bot"),
            ]),
        ]
    };
    let hits = if menu.hits.is_empty() {
        String::new()
    } else {
        menu.hits
            .iter()
            .enumerate()
            .map(|(i, h)| format!("\n{}. **{}** — {}", i + 1, h.title, h.detail))
            .collect::<Vec<_>>()
            .join("")
    };
    EditInteractionResponse::new()
        .content(clipped(
            &format!(
                "**{}** · {location}{capability}\n{}\n{notice}{hits}",
                endpoint.name, status.now
            ),
            1900,
        ))
        .components(rows)
}

impl Handler {
    fn front_input_on_cooldown(&self, user: UserId, action: &Action) -> bool {
        // Search admission is limited by its performer. Direct links/files are
        // limited here, before the picker. Choosing an already-issued search
        // result must not spend the same cooldown a second time.
        matches!(action, Action::Play { .. })
            && needs_media(action)
            && self.media_lookup_on_cooldown(user)
    }

    async fn front_status(&self, endpoint: &mut Endpoint, user: u64) {
        let req = transport::request(self.guild_id.get(), user, Action::Status);
        endpoint.status =
            match tokio::time::timeout(Duration::from_secs(5), endpoint.client.call(&req)).await {
                Ok(Ok(Reply::Status(status)))
                    if status.guild == self.guild_id.get()
                        && status.ready
                        && status.name == endpoint.name =>
                {
                    Some(status)
                }
                _ => None,
            };
    }

    async fn front_menu(&self, ctx: &Context, user: UserId, pending: Option<Action>) -> Menu {
        let cfg = &self.config.routing;
        let key = cfg.key.expect("validated routing key");
        let mut spotibot = Endpoint {
            client: Client::new(cfg.peer.unwrap(), key),
            name: "Spotibot",
            status: None,
        };
        let mut nob = Endpoint {
            client: Client::new(cfg.listen.unwrap(), key),
            name: "nob",
            status: None,
        };
        tokio::join!(
            self.front_status(&mut spotibot, user.get()),
            self.front_status(&mut nob, user.get())
        );
        let room = self.voice_channels(ctx, user).1.map(|room| room.get());
        let endpoints = vec![spotibot, nob];
        let statuses: Option<Vec<_>> = endpoints
            .iter()
            .map(|endpoint| endpoint.status.clone())
            .collect();
        let selected = statuses.and_then(|bots| {
            routing::choose(
                room,
                &bots,
                pending.is_some(),
                pending.as_ref().is_some_and(needs_media),
            )
            .ok()
        });
        Menu {
            user: user.get(),
            guild: self.guild_id.get(),
            room,
            expires: Instant::now() + TTL,
            endpoints,
            selected,
            pending,
            hits: Vec::new(),
            queued: false,
            next: false,
        }
    }

    async fn front_action(&self, menu: &Menu, action: Action) -> Reply {
        let Some(endpoint) = menu.selected.and_then(|i| menu.endpoints.get(i)) else {
            return Reply::Error("Choose a bot first.".into());
        };
        let Some(status) = &endpoint.status else {
            return Reply::Error("That bot is unavailable.".into());
        };
        let mut req = transport::request(menu.guild, menu.user, action);
        req.room = menu.room;
        req.target = Some(status.target.clone());
        match endpoint.client.call(&req).await {
            Ok(reply) => reply,
            Err(_) => {
                // No fallback to another performer: the first may already have
                // acted. Query its retained result without repeating the action.
                let mut query =
                    transport::request(menu.guild, menu.user, Action::Result { request: req.id });
                query.target = req.target;
                match tokio::time::timeout(Duration::from_secs(5), endpoint.client.call(&query)).await {
                    Ok(Ok(reply)) => reply,
                    _ => Reply::Error("The bot disconnected before confirming. Check its playback card before trying again.".into()),
                }
            }
        }
    }

    async fn front_render(
        &self,
        ctx: &Context,
        interaction: &InteractionRef<'_>,
        mut menu: Menu,
        notice: &str,
    ) {
        if let Some(index) = menu.selected {
            self.front_status(&mut menu.endpoints[index], menu.user)
                .await;
            if menu.endpoints[index].status.is_none() {
                menu.selected = None;
            }
        }
        let id = self.front_menus.lock().insert(menu.clone());
        interaction.edit(ctx, panel(&id, &menu, notice)).await;
    }

    async fn front_run(
        &self,
        ctx: &Context,
        interaction: &InteractionRef<'_>,
        mut menu: Menu,
        action: Action,
    ) {
        if let Action::Search { next, queued, .. } = &action {
            menu.next = *next;
            menu.queued = *queued;
        }
        let reply = self.front_action(&menu, action).await;
        let notice = match reply {
            Reply::Text(text) | Reply::Error(text) => text,
            Reply::Search(hits) => { menu.hits = hits; if menu.hits.is_empty() { "No matching tracks found.".into() } else { "Choose a track below.".into() } },
            Reply::Pairing { url, code, pairing } => {
                interaction.text(ctx, &format!("**{}** Spotify login\nGo to <{url}> and enter **{code}**. This code expires in ten minutes.", menu.endpoints[menu.selected.unwrap()].name)).await;
                match self.front_action(&menu, Action::FinishLogin { pairing }).await {
                    Reply::Text(text) | Reply::Error(text) => text,
                    _ => "Login is still completing. Open the account panel to check.".into(),
                }
            }
            Reply::Pending => "The original request is still completing. Check the playback card before trying again.".into(),
            Reply::Status(_) => String::new(),
        };
        let notice = menu_help(&notice);
        // A search selection keeps the generation it searched: refreshing it
        // would authorize a delayed selection against a replacement session.
        if !menu.hits.is_empty() {
            let id = self.front_menus.lock().insert(menu.clone());
            interaction.edit(ctx, panel(&id, &menu, &notice)).await;
        } else {
            self.front_render(ctx, interaction, menu, &notice).await;
        }
    }

    pub(super) async fn dispatch_front(&self, ctx: &Context, interaction: &Interaction) -> bool {
        if self.config.routing.mode != CommandMode::Coordinator {
            return false;
        }
        if super::admin::handle_panel(ctx, interaction, self.config.profile, self.guild_id).await {
            return true;
        }
        match interaction {
            Interaction::Command(cmd)
                if cmd.guild_id == Some(self.guild_id)
                    && matches!(cmd.data.name.as_str(), "music" | "play") =>
            {
                let response = InteractionRef::Command(cmd);
                if !response.defer(ctx).await {
                    return true;
                }
                let pending = if cmd.data.name == "play" {
                    let input = cmd
                        .data
                        .options
                        .iter()
                        .find(|o| o.name == "query")
                        .and_then(|o| o.value.as_str())
                        .map(str::to_owned);
                    let attachment =
                        cmd.data
                            .resolved
                            .attachments
                            .values()
                            .next()
                            .map(|a| Attachment {
                                filename: a.filename.clone(),
                                url: a.url.clone(),
                                size: a.size as u64,
                            });
                    let next = cmd
                        .data
                        .options
                        .iter()
                        .find(|o| o.name == "next")
                        .and_then(|o| o.value.as_bool())
                        .unwrap_or(false);
                    if input.is_some() && attachment.is_some() {
                        response
                            .text(ctx, "Provide a song/link or an attachment, not both.")
                            .await;
                        return true;
                    }
                    Some(input_action(input, attachment, next, false))
                } else {
                    None
                };
                if pending
                    .as_ref()
                    .is_some_and(|action| self.front_input_on_cooldown(cmd.user.id, action))
                {
                    response.text(ctx, "Try again in a few seconds.").await;
                    return true;
                }
                let mut menu = self.front_menu(ctx, cmd.user.id, pending).await;
                if menu.selected.is_some() {
                    if let Some(action) = menu.pending.take() {
                        self.front_run(ctx, &response, menu, action).await;
                    } else {
                        self.front_render(ctx, &response, menu, "").await;
                    }
                } else {
                    self.front_render(ctx, &response, menu, "").await;
                }
                true
            }
            Interaction::Component(component) if component.data.custom_id.starts_with("route:") => {
                let response = InteractionRef::Component(component);
                let parts: Vec<_> = component.data.custom_id.split(':').collect();
                let menu = if parts.len() == 3 && component.guild_id == Some(self.guild_id) {
                    self.front_menus.lock().take(
                        parts[1],
                        component.user.id.get(),
                        self.guild_id.get(),
                    )
                } else {
                    None
                };
                let Some(mut menu) = menu else {
                    if response.defer(ctx).await {
                        response
                            .text(
                                ctx,
                                "This menu expired or was already used. Open /music again.",
                            )
                            .await;
                    }
                    return true;
                };
                let action = parts[2];
                if matches!(action, "add" | "enqueue") {
                    menu.queued = action == "enqueue";
                    let id = self.front_menus.lock().insert(menu);
                    let modal = CreateModal::new(format!("route:{id}:input"), "Add music")
                        .components(vec![CreateActionRow::InputText(
                            CreateInputText::new(
                                InputTextStyle::Short,
                                "Song name or music link",
                                "query",
                            )
                            .max_length(4096)
                            .required(true),
                        )]);
                    let _ = component
                        .create_response(ctx, CreateInteractionResponse::Modal(modal))
                        .await;
                    return true;
                }
                if !response.defer(ctx).await {
                    return true;
                }
                if action == "choose" {
                    menu.selected = None;
                    menu.pending = None;
                    self.front_render(ctx, &response, menu, "").await;
                    return true;
                }
                if let Some(index) = action
                    .strip_prefix("select")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if menu.endpoints.get(index).is_none_or(|e| e.status.is_none()) {
                        response.text(ctx, "That bot is unavailable.").await;
                        return true;
                    }
                    menu.selected = Some(index);
                    if let Some(action) = menu.pending.take() {
                        self.front_run(ctx, &response, menu, action).await;
                    } else {
                        self.front_render(ctx, &response, menu, "").await;
                    }
                    return true;
                }
                if let Some(index) = action
                    .strip_prefix("pick")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    let Some(hit) = menu.hits.get(index).cloned() else {
                        response.text(ctx, "That result expired.").await;
                        return true;
                    };
                    menu.hits.clear();
                    let queued = menu.queued;
                    let next = menu.next;
                    self.front_run(
                        ctx,
                        &response,
                        menu,
                        Action::Play {
                            input: Some(hit.url),
                            attachment: None,
                            next,
                            queued,
                        },
                    )
                    .await;
                    return true;
                }
                if matches!(action, "clear" | "forget") {
                    let id = self.front_menus.lock().insert(menu);
                    response
                        .edit(
                            ctx,
                            EditInteractionResponse::new()
                                .content(if action == "clear" {
                                    "Clear this bot's queue? Current playback keeps going."
                                } else {
                                    "Permanently delete your Spotify login from this bot?"
                                })
                                .components(vec![CreateActionRow::Buttons(vec![
                                    button(&id, &format!("confirm_{action}"), "Confirm"),
                                    button(&id, "refresh", "Cancel"),
                                ])]),
                        )
                        .await;
                    return true;
                }
                let operation = match action {
                    "play" => Some(Action::Control(Control::Play)),
                    "pause" => Some(Action::Control(Control::Pause)),
                    "previous" => Some(Action::Control(Control::Previous)),
                    "skip" => Some(Action::Control(Control::Skip)),
                    "stop" => Some(Action::Control(Control::Stop)),
                    "confirm_clear" => Some(Action::Control(Control::Clear)),
                    "announce" => Some(Action::Control(Control::Announce)),
                    "queue" => Some(Action::View(View::Queue)),
                    "history" => Some(Action::View(View::History)),
                    "account" => Some(Action::View(View::Account)),
                    "login" => Some(Action::Login),
                    "logout" => Some(Action::Logout),
                    "confirm_forget" => Some(Action::Forget),
                    _ => None,
                };
                if let Some(action) = operation {
                    self.front_run(ctx, &response, menu, action).await;
                } else {
                    self.front_render(ctx, &response, menu, "").await;
                }
                true
            }
            Interaction::Modal(modal) if modal.data.custom_id.starts_with("route:") => {
                let response = InteractionRef::Modal(modal);
                if !response.defer(ctx).await {
                    return true;
                }
                let parts: Vec<_> = modal.data.custom_id.split(':').collect();
                let menu = if parts.len() == 3
                    && parts[2] == "input"
                    && modal.guild_id == Some(self.guild_id)
                {
                    self.front_menus
                        .lock()
                        .take(parts[1], modal.user.id.get(), self.guild_id.get())
                } else {
                    None
                };
                let Some(menu) = menu else {
                    response
                        .text(ctx, "This menu expired. Open /music again.")
                        .await;
                    return true;
                };
                let input = modal
                    .data
                    .components
                    .iter()
                    .flat_map(|row| &row.components)
                    .find_map(|c| match c {
                        ActionRowComponent::InputText(input) if input.custom_id == "query" => {
                            input.value.clone()
                        }
                        _ => None,
                    });
                if input.as_ref().is_none_or(|text| text.trim().is_empty()) {
                    response.text(ctx, "Enter a song name or music link.").await;
                    return true;
                }
                let queued = menu.queued;
                let action = input_action(input, None, false, queued);
                if self.front_input_on_cooldown(modal.user.id, &action) {
                    response.text(ctx, "Try again in a few seconds.").await;
                    return true;
                }
                self.front_run(ctx, &response, menu, action).await;
                true
            }
            _ => false,
        }
    }
}

fn input_action(
    input: Option<String>,
    attachment: Option<Attachment>,
    next: bool,
    queued: bool,
) -> Action {
    if let Some(query) = input.as_ref().filter(|s| {
        !s.trim().starts_with("https://")
            && !s.trim().starts_with("http://")
            && !s.trim().starts_with("spotify:")
    }) {
        Action::Search {
            query: query.trim().into(),
            next,
            queued,
        }
    } else {
        Action::Play {
            input,
            attachment,
            next,
            queued,
        }
    }
}

fn needs_media(action: &Action) -> bool {
    match action {
        Action::Search { .. } => true,
        Action::Play {
            input, attachment, ..
        } => {
            attachment.is_some()
                || input.as_ref().is_some_and(|input| {
                    !matches!(
                        super::commands::classify_link(input),
                        super::commands::LinkKind::Spotify(_)
                    )
                })
        }
        _ => false,
    }
}

fn menu_help(text: &str) -> String {
    let mut text = text.to_string();
    for (command, label) in [
        ("login", "Log in"),
        ("logout", "Log out"),
        ("forget", "Forget login"),
        ("who", "Spotify account"),
        ("queue", "Queue"),
        ("clear", "Clear queue"),
        ("history", "History"),
        ("np", "Refresh"),
        ("skip", "Skip"),
        ("stop", "Stop"),
        ("announce", "Announcements"),
    ] {
        text = text.replace(
            &format!("`/{command}`"),
            &format!("**{label}** in `/music`"),
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        Menu {
            user: 1,
            guild: 2,
            room: Some(3),
            expires: Instant::now() + TTL,
            endpoints: vec![],
            selected: None,
            pending: None,
            hits: vec![],
            queued: false,
            next: false,
        }
    }

    #[test]
    fn private_menus_are_owner_guild_expiry_bound_and_single_use() {
        let mut menus = Menus::default();
        let id = menus.insert(menu());
        assert!(menus.take(&id, 9, 2).is_none());
        assert!(menus.take(&id, 1, 9).is_none());
        assert!(menus.take(&id, 1, 2).is_some());
        assert!(menus.take(&id, 1, 2).is_none());
        let id = menus.insert(menu());
        menus.entries.get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(menus.take(&id, 1, 2).is_none());
        for _ in 0..MAX_MENUS + 1 {
            menus.insert(menu());
        }
        assert_eq!(menus.entries.len(), MAX_MENUS);
    }

    #[test]
    fn compact_surface_and_search_keep_all_track_options() {
        let commands = serde_json::to_value(register_commands()).unwrap();
        let names: Vec<_> = commands
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["play", "music", "server", "soundboard"]);
        let action = input_action(Some("  song name  ".into()), None, true, true);
        assert!(
            matches!(&action, Action::Search { query, next: true, queued: true } if query == "song name")
        );
        assert!(needs_media(&action));
        let action = input_action(
            Some("spotify:track:0000000000000000000001".into()),
            None,
            false,
            false,
        );
        assert!(!needs_media(&action));
        assert!(!needs_media(&input_action(None, None, false, false)));
        assert!(needs_media(&input_action(
            Some("https://youtu.be/test".into()),
            None,
            false,
            false
        )));
        assert!(menu_help("Run `/login` then `/queue`.").contains("**Log in** in `/music`"));
    }
}
