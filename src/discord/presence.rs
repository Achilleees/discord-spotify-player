use crate::presence::PresenceUpdate;
use serenity::client::Context;
use serenity::gateway::ActivityData;
use serenity::model::user::OnlineStatus;
use std::time::Duration;
use tokio::sync::mpsc;

fn status_text(state: &PresenceUpdate, dance_flip: bool) -> String {
    match state {
        PresenceUpdate::Idle => "Idle".to_string(),
        PresenceUpdate::Paused => "Paused".to_string(),
        PresenceUpdate::Playing { title, artist } => {
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
        PresenceUpdate::Paused | PresenceUpdate::Idle => OnlineStatus::Idle,
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
