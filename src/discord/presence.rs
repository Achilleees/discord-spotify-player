use crate::presence::PresenceUpdate;
use serenity::client::Context;
use serenity::gateway::ActivityData;
use serenity::model::user::OnlineStatus;
use std::time::Duration;
use tokio::sync::mpsc;

fn status_text(state: &PresenceUpdate, dance_flip: bool) -> String {
    match state {
        PresenceUpdate::Idle => "Idle".to_string(),
        PresenceUpdate::Paused { .. } => "Paused".to_string(),
        PresenceUpdate::Playing { title, artist, .. } => {
            let note = if dance_flip { "\u{266A}" } else { "\u{266C}" };
            let base = format!("{note} {title} - {artist}");
            truncate_status(&base, 96)
        }
    }
}

fn truncate_status(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx + 3 >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

async fn set_presence(ctx: &Context, state: &PresenceUpdate, dance_flip: bool) {
    let text = status_text(state, dance_flip);
    let activity = ActivityData::custom(text);
    let status = match state {
        PresenceUpdate::Playing { .. } => OnlineStatus::Online,
        PresenceUpdate::Paused { .. } | PresenceUpdate::Idle => OnlineStatus::Idle,
    };
    ctx.set_presence(Some(activity), status);
}

pub async fn run_presence_loop(ctx: Context, mut rx: mpsc::UnboundedReceiver<PresenceUpdate>) {
    let mut state = PresenceUpdate::Idle;
    let mut dance_flip = false;
    let mut interval = tokio::time::interval(Duration::from_secs(12));
    set_presence(&ctx, &state, dance_flip).await;

    loop {
        tokio::select! {
            Some(update) = rx.recv() => {
                state = update;
                dance_flip = false;
                set_presence(&ctx, &state, dance_flip).await;
            }
            _ = interval.tick(), if matches!(state, PresenceUpdate::Playing { .. }) => {
                dance_flip = !dance_flip;
                set_presence(&ctx, &state, dance_flip).await;
            }
            else => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{status_text, truncate_status};
    use crate::presence::PresenceUpdate;

    fn playing(title: &str, artist: &str) -> PresenceUpdate {
        PresenceUpdate::Playing {
            title: title.to_string(),
            artist: artist.to_string(),
            track_id: "t".to_string(),
            album_art_url: None,
        }
    }

    fn paused(title: &str, artist: &str) -> PresenceUpdate {
        PresenceUpdate::Paused {
            title: title.to_string(),
            artist: artist.to_string(),
            track_id: "t".to_string(),
        }
    }

    #[test]
    fn maps_all_three_states() {
        assert_eq!(status_text(&PresenceUpdate::Idle, false), "Idle");
        assert_eq!(status_text(&paused("Song", "Artist"), false), "Paused");
        assert_eq!(
            status_text(&playing("Song", "Artist"), false),
            "\u{266C} Song - Artist"
        );
    }

    #[test]
    fn dance_flip_alternates_the_note() {
        let p = playing("Song", "Artist");
        assert!(status_text(&p, true).starts_with('\u{266A}'));
        assert!(status_text(&p, false).starts_with('\u{266C}'));
        // The flip only animates Playing; Idle/Paused stay untouched.
        assert_eq!(status_text(&PresenceUpdate::Idle, true), "Idle");
    }

    #[test]
    fn playing_status_is_truncated_to_discord_limit() {
        let long = status_text(&playing(&"x".repeat(200), "Artist"), false);
        assert!(long.chars().count() <= 96);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(truncate_status("hi", 96), "hi");
    }

    #[test]
    fn text_at_limit_is_unchanged() {
        let s = "x".repeat(96);
        assert_eq!(truncate_status(&s, 96), s);
    }

    #[test]
    fn long_text_is_truncated_with_ellipsis() {
        let out = truncate_status(&"x".repeat(200), 96);
        assert!(out.ends_with("..."));
        assert!(out.chars().count() <= 96, "got {} chars", out.chars().count());
    }

    #[test]
    fn truncation_is_char_safe_on_multibyte() {
        // Must not panic or split a multibyte char mid-way.
        let out = truncate_status(&"🎵".repeat(200), 96);
        assert!(out.ends_with("..."));
    }
}
