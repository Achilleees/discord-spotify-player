//! Performer-side execution of typed requests. Every process checks its own
//! current Discord voice state and keeps Spotify credentials and pairing polls.

use super::{
    account::LoginOutcome,
    bot::Handler,
    commands::{render_now_playing, TrackRequest},
    ui::clipped,
};
use crate::{
    oauth::DeviceAuthorization,
    routing::{Action, Control, Reply, Request, SearchHit, Status, Target, View},
};
use serenity::all::{Context, UserId};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(super) struct Pairing {
    pub user: u64,
    pub auth: DeviceAuthorization,
    pub expires: Instant,
    pub permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Default)]
pub(super) struct Pairings {
    entries: HashMap<String, Pairing>,
}

impl Pairings {
    fn insert(
        &mut self,
        user: u64,
        auth: DeviceAuthorization,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Option<String> {
        self.entries
            .retain(|_, p| p.user != user && p.expires > Instant::now());
        if self.entries.len() >= 16 {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.entries.insert(
            id.clone(),
            Pairing {
                user,
                auth,
                expires: Instant::now() + Duration::from_secs(600),
                permit,
            },
        );
        Some(id)
    }
    fn take(&mut self, id: &str, user: u64) -> Option<Pairing> {
        if self
            .entries
            .get(id)
            .is_none_or(|p| p.user != user || p.expires <= Instant::now())
        {
            return None;
        }
        self.entries.remove(id)
    }
    fn cancel(&mut self, user: u64) {
        self.entries
            .retain(|_, p| p.user != user && p.expires > Instant::now());
    }
}

impl Handler {
    pub(super) fn voice_guard_current(&self, guard: &crate::player::state::VoiceGuard) -> bool {
        let Some(ctx) = self.ctx.lock().clone() else {
            return false;
        };
        let (bot, user) = self.voice_channels(&ctx, UserId::new(guard.user));
        guard.allows(
            self.voice_owner.snapshot().0,
            bot.map(|ch| ch.get()),
            user.map(|ch| ch.get()),
        )
    }

    pub(super) fn music_target(&self, ctx: &Context) -> Target {
        let (generation, claimed) = self.voice_owner.snapshot();
        let actual = self.guild_id.to_guild_cached(ctx).and_then(|guild| {
            guild
                .voice_states
                .get(&ctx.cache.current_user().id)
                .and_then(|vs| vs.channel_id)
                .map(|ch| ch.get())
        });
        Target {
            boot: self.boot.clone(),
            generation,
            room: claimed.or(actual),
        }
    }

    pub(super) fn target_current(&self, ctx: &Context, target: &Target) -> bool {
        *target == self.music_target(ctx)
    }

    pub(super) fn routed_voice_authorized(
        &self,
        ctx: &Context,
        user: UserId,
        target: &Target,
        room: Option<u64>,
        may_join: bool,
    ) -> bool {
        self.target_current(ctx, target)
            && room.is_some()
            && self.voice_channels(ctx, user).1.map(|ch| ch.get()) == room
            && (target.room == room || (may_join && target.room.is_none()))
    }

    pub(super) async fn execute_routed(&self, request: Request) -> Reply {
        if request.guild != self.guild_id.get() {
            return Reply::Error("This bot serves a different server.".into());
        }
        let Some(ctx) = self.ctx.lock().clone() else {
            return Reply::Error("This bot is still connecting.".into());
        };
        if matches!(request.action, Action::Status) {
            let now = clipped(&render_now_playing(&self.player.query().await.now), 350);
            return Reply::Status(Status {
                target: self.music_target(&ctx),
                name: self.config.profile.name().into(),
                guild: self.guild_id.get(),
                bot: ctx.cache.current_user().id.get(),
                ready: self.guild_id.to_guild_cached(&ctx).is_some(),
                media: self.ytdlp_available,
                now,
            });
        }
        let Some(target) = request.target.as_ref() else {
            return Reply::Error("Choose a bot first.".into());
        };
        if target.boot != self.boot {
            return Reply::Error("This bot restarted. Open a fresh menu.".into());
        }
        let user_id = UserId::new(request.user);
        let user_text = request.user.to_string();
        let login_guard = crate::player::state::VoiceGuard {
            generation: target.generation,
            room: request.room.unwrap_or(0),
            user: request.user,
            may_join: true,
        };
        let voice =
            |may_join| self.routed_voice_authorized(&ctx, user_id, target, request.room, may_join);
        match request.action {
            Action::Status | Action::Result { .. } => Reply::Error("Unexpected request.".into()),
            Action::View(view) => Reply::Text(match view {
                View::Now => render_now_playing(&self.player.query().await.now),
                View::Queue => self.format_queue_listing().await,
                View::History => self.handle_history(10).await,
                View::Account => self.handle_who().await,
            }),
            Action::Search { query, .. } => {
                if !voice(true) {
                    return Reply::Error(
                        "Your room or the music session changed. Open a fresh menu.".into(),
                    );
                }
                if !self.ytdlp_available || self.media_lookup_on_cooldown(user_id) {
                    return Reply::Error(
                        "Search is unavailable or busy. Try again shortly.".into(),
                    );
                }
                match crate::youtube::metadata::search_youtube(&query).await {
                    Ok(hits) => Reply::Search(
                        hits.into_iter()
                            .map(|hit| SearchHit {
                                title: clipped(&hit.title, 80),
                                detail: clipped(&hit.channel, 100),
                                url: hit.url,
                            })
                            .collect(),
                    ),
                    Err(e) => Reply::Error(e.to_string()),
                }
            }
            Action::Play {
                input,
                attachment,
                next,
                queued,
            } => {
                if !voice(!queued) {
                    return Reply::Error(
                        "Your room or the music session changed. Open a fresh menu.".into(),
                    );
                }
                if input.as_ref().is_some_and(|s| s.len() > 4096)
                    || (input.is_some() && attachment.is_some())
                {
                    return Reply::Error("Provide one music link or file.".into());
                }
                if let Some(file) = &attachment {
                    if !valid_attachment_url(&file.url) {
                        return Reply::Error("Unsupported attachment address.".into());
                    }
                }
                let Ok(user) = user_id.to_user(&ctx.http).await else {
                    return Reply::Error("Couldn't identify the requester.".into());
                };
                if input.is_none() && attachment.is_none() {
                    if !voice(true) {
                        return Reply::Error(
                            "The music session changed. Open a fresh menu.".into(),
                        );
                    }
                    return Reply::Text(
                        self.player
                            .guarded(crate::player::state::VoiceGuard {
                                generation: target.generation,
                                room: request.room.unwrap(),
                                user: request.user,
                                may_join: true,
                            })
                            .play()
                            .await,
                    );
                }
                Reply::Text(
                    self.add_track(
                        &ctx,
                        &user,
                        TrackRequest {
                            url: input,
                            attachment,
                            next,
                            start_if_idle: !queued,
                        },
                        Some((target, request.room)),
                    )
                    .await,
                )
            }
            Action::Control(control) => {
                if control != Control::Announce
                    && !voice(matches!(control, Control::Play | Control::Clear))
                {
                    return Reply::Error(
                        "Join the selected bot's room and open a fresh menu.".into(),
                    );
                }
                let player = if let Some(room) = request.room {
                    self.player.guarded(crate::player::state::VoiceGuard {
                        generation: target.generation,
                        room,
                        user: request.user,
                        may_join: matches!(control, Control::Play | Control::Clear),
                    })
                } else {
                    self.player.clone()
                };
                Reply::Text(match control {
                    Control::Play => player.play().await,
                    Control::Previous => player.previous().await,
                    Control::Skip => player.skip().await,
                    Control::Pause => player.toggle_pause().await,
                    Control::Stop => player.stop().await,
                    Control::Clear => player.clear_queue().await,
                    Control::Announce => self.handle_announce().await,
                })
            }
            Action::Login => {
                self.pairings.lock().cancel(request.user);
                if let Some(cancel) = self.pending_auth.lock().remove(&request.user) {
                    cancel.notify_one();
                }
                let Ok(permit) = self.pairing_slots.clone().try_acquire_owned() else {
                    return Reply::Error("Spotify login is busy. Try again shortly.".into());
                };
                let Ok(user) = user_id.to_user(&ctx.http).await else {
                    return Reply::Error("Couldn't identify the requester.".into());
                };
                let username = user.global_name.as_deref().unwrap_or(&user.name);
                match self
                    .handle_login(
                        &user_text,
                        request.user,
                        username,
                        voice(false),
                        Some(login_guard),
                    )
                    .await
                {
                    LoginOutcome::Reply(text) => Reply::Text(text),
                    LoginOutcome::Pair(auth) => {
                        let url = auth.url().to_string();
                        let code = auth.user_code.clone();
                        match self.pairings.lock().insert(request.user, auth, permit) {
                            Some(pairing) => Reply::Pairing { url, code, pairing },
                            None => Reply::Error(
                                "Too many Spotify pairings are pending. Try again shortly.".into(),
                            ),
                        }
                    }
                }
            }
            Action::FinishLogin { pairing } => {
                let auth = self.pairings.lock().take(&pairing, request.user);
                let Some(Pairing {
                    auth,
                    permit: _permit,
                    ..
                }) = auth
                else {
                    return Reply::Error("This pairing expired or was already used.".into());
                };
                let Ok(user) = user_id.to_user(&ctx.http).await else {
                    return Reply::Error("Couldn't identify the requester.".into());
                };
                let username = user.global_name.as_deref().unwrap_or(&user.name);
                Reply::Text(
                    self.finish_device_login(
                        &user_text,
                        request.user,
                        username,
                        &ctx,
                        auth,
                        Some(login_guard),
                    )
                    .await,
                )
            }
            Action::Logout => {
                self.pairings.lock().cancel(request.user);
                Reply::Text(self.handle_logout(&user_text, request.user).await)
            }
            Action::Forget => {
                self.pairings.lock().cancel(request.user);
                Reply::Text(self.handle_forget(&user_text, request.user).await)
            }
        }
    }
}

fn valid_attachment_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str(),
                Some("cdn.discordapp.com" | "media.discordapp.net")
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attachments_only_use_discord_https_cdn() {
        assert!(valid_attachment_url(
            "https://cdn.discordapp.com/attachments/file.mp3?signature=test"
        ));
        for url in [
            "http://cdn.discordapp.com/x",
            "https://cdn.discordapp.com.evil.test/x",
            "file:///x",
            "https://127.0.0.1/x",
            "https://cdn.discordapp.com:123/x",
        ] {
            assert!(!valid_attachment_url(url));
        }
    }
    #[test]
    fn pairing_is_owner_bound_single_use_and_releases_capacity() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let auth = || DeviceAuthorization {
            device_code: "synthetic-device-code".into(),
            user_code: "synthetic-user-code".into(),
            verification_uri: "https://example.invalid/pair".into(),
            verification_uri_complete: None,
            expires_in: 600,
            interval: 5,
        };
        let mut pairings = Pairings::default();
        let id = pairings
            .insert(1, auth(), slots.clone().try_acquire_owned().unwrap())
            .unwrap();
        assert!(pairings.take(&id, 2).is_none());
        assert_eq!(slots.available_permits(), 0);
        drop(pairings.take(&id, 1).unwrap());
        assert!(pairings.take(&id, 1).is_none());
        assert_eq!(slots.available_permits(), 1);
        let id = pairings
            .insert(1, auth(), slots.clone().try_acquire_owned().unwrap())
            .unwrap();
        pairings.entries.get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(pairings.take(&id, 1).is_none());
        pairings.cancel(2); // Any subsequent login also reaps expired pairings.
        assert_eq!(slots.available_permits(), 1);
    }
}
