//! Nob's first server utilities, adapted from legacy nob-admin.
//! Permission checks are enforced on invocation as well as command registration.
use crate::runtime::Profile;
use serenity::all::*;

pub(super) fn register_commands(profile: Profile) -> Vec<CreateCommand> {
    if profile != Profile::Nob {
        return Vec::new();
    }
    vec![
        CreateCommand::new("slowmode")
            .description("Set this channel's slowmode (0 disables it)")
            .default_member_permissions(Permissions::MANAGE_CHANNELS)
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "seconds",
                    "Interval in seconds (0-21600)",
                )
                .required(true)
                .min_int_value(0)
                .max_int_value(21600),
            ),
        CreateCommand::new("purge")
            .description("Clean recent messages, keeping pins and bot messages")
            .default_member_permissions(Permissions::MANAGE_MESSAGES)
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "count",
                    "Recent messages to inspect (1-100); old messages are skipped",
                )
                .required(true)
                .min_int_value(1)
                .max_int_value(100),
            ),
    ]
}

fn permission_for(name: &str) -> Option<Permissions> {
    match name {
        "slowmode" => Some(Permissions::MANAGE_CHANNELS),
        "purge" => Some(
            Permissions::MANAGE_MESSAGES
                | Permissions::VIEW_CHANNEL
                | Permissions::READ_MESSAGE_HISTORY,
        ),
        _ => None,
    }
}

fn authorized(
    profile: Profile,
    expected_guild: GuildId,
    actual_guild: Option<GuildId>,
    actor: Option<Permissions>,
    bot: Option<Permissions>,
    required: Permissions,
) -> bool {
    let has = |permissions: Option<Permissions>| {
        permissions.is_some_and(|p| p.contains(Permissions::ADMINISTRATOR) || p.contains(required))
    };
    profile == Profile::Nob && actual_guild == Some(expected_guild) && has(actor) && has(bot)
}

fn integer_option(cmd: &CommandInteraction, name: &str, min: i64, max: i64) -> Option<i64> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| o.value.as_i64())
        .filter(|n| (min..=max).contains(n))
}

fn purge_eligible(pinned: bool, bot_message: bool, timestamp: i64, now: i64) -> bool {
    // Preserve all bot/webhook messages: without MESSAGE_CONTENT, another
    // bot's components may be absent even on an active playback card.
    // A two-minute margin keeps a boundary-age message from crossing Discord's
    // bulk-delete limit between fetching the batch and executing the request.
    !pinned && !bot_message && timestamp > now - (14 * 24 * 60 * 60 - 120)
}

pub(super) async fn handle(
    ctx: &Context,
    cmd: &CommandInteraction,
    profile: Profile,
    guild: GuildId,
) -> bool {
    let Some(required) = permission_for(&cmd.data.name) else {
        return false;
    };
    let permitted = authorized(
        profile,
        guild,
        cmd.guild_id,
        cmd.member.as_ref().and_then(|m| m.permissions),
        cmd.app_permissions,
        required,
    );
    if !permitted {
        let response = CreateInteractionResponse::Message(CreateInteractionResponseMessage::new()
            .content("This command requires the appropriate channel permissions for both you and nob.")
            .ephemeral(true));
        let _ = cmd.create_response(ctx, response).await;
        return true;
    }
    if cmd.defer_ephemeral(ctx).await.is_err() {
        return true;
    }
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(20), execute(ctx, cmd, guild)).await;
    let content = match result {
        Ok(Ok(message)) => message,
        Ok(Err(error)) => {
            tracing::warn!(command = cmd.data.name, error = %error, "server utility failed");
            "Couldn't confirm the change. Check the channel and nob's permissions before retrying.".into()
        }
        Err(_) => "The request timed out. Check the channel before retrying; the change may have completed.".into(),
    };
    let _ = cmd
        .edit_response(
            ctx,
            EditInteractionResponse::new()
                .content(content)
                .allowed_mentions(CreateAllowedMentions::new()),
        )
        .await;
    true
}

async fn execute(
    ctx: &Context,
    cmd: &CommandInteraction,
    guild: GuildId,
) -> Result<String, Box<serenity::Error>> {
    if cmd.data.name == "slowmode" {
        let Some(seconds) = integer_option(cmd, "seconds", 0, 21600) else {
            return Ok("Choose a slowmode interval from 0 to 21600 seconds.".into());
        };
        let channel = cmd.channel_id.to_channel(&ctx.http).await?;
        let Channel::Guild(channel) = channel else {
            return Ok("Use this in a server text or voice channel.".into());
        };
        if channel.guild_id != guild
            || !matches!(
                channel.kind,
                ChannelType::Text | ChannelType::News | ChannelType::Voice | ChannelType::Stage
            )
        {
            return Ok(
                "Use this in a server text or voice channel; thread slowmode is not supported yet."
                    .into(),
            );
        }
        cmd.channel_id
            .edit(
                &ctx.http,
                EditChannel::new()
                    .rate_limit_per_user(seconds as u16)
                    .audit_log_reason(&format!("slowmode requested by {}", cmd.user.id)),
            )
            .await?;
        Ok(if seconds == 0 {
            "Slowmode disabled.".into()
        } else {
            format!("Slowmode set to {seconds} seconds.")
        })
    } else {
        let Some(count) = integer_option(cmd, "count", 1, 100) else {
            return Ok("Choose between 1 and 100 recent messages to inspect.".into());
        };
        let messages = cmd
            .channel_id
            .messages(&ctx.http, GetMessages::new().limit(count as u8))
            .await?;
        let now = Timestamp::now().unix_timestamp();
        let ids: Vec<_> = messages
            .iter()
            .filter(|m| {
                purge_eligible(
                    m.pinned,
                    m.author.bot || m.webhook_id.is_some(),
                    m.timestamp.unix_timestamp(),
                    now,
                )
            })
            .map(|m| m.id)
            .collect();
        if !ids.is_empty() {
            // Serenity uses the single-delete endpoint when exactly one remains.
            cmd.channel_id.delete_messages(&ctx.http, &ids).await?;
        }
        tracing::info!(actor = %cmd.user.id, channel = %cmd.channel_id, deleted = ids.len(), "channel cleanup completed");
        Ok(format!(
            "Deleted {} message(s). Kept {} pinned, older or bot/webhook message(s).",
            ids.len(),
            messages.len() - ids.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utilities_are_registered_only_for_nob() {
        assert!(register_commands(Profile::Spotibot).is_empty());
        let commands = serde_json::to_value(register_commands(Profile::Nob)).unwrap();
        assert_eq!(commands[0]["name"], "slowmode");
        assert_eq!(commands[1]["name"], "purge");
        assert_eq!(commands[1]["options"][0]["max_value"], 100);
    }

    #[test]
    fn invocation_checks_identity_guild_and_both_permissions() {
        let guild = GuildId::new(1);
        let needed = permission_for("purge").unwrap();
        let permitted =
            |profile, actual, user, bot| authorized(profile, guild, actual, user, bot, needed);
        assert!(permitted(
            Profile::Nob,
            Some(guild),
            Some(needed),
            Some(needed)
        ));
        assert!(permitted(
            Profile::Nob,
            Some(guild),
            Some(Permissions::ADMINISTRATOR),
            Some(needed)
        ));
        assert!(!permitted(
            Profile::Spotibot,
            Some(guild),
            Some(needed),
            Some(needed)
        ));
        assert!(!permitted(Profile::Nob, None, Some(needed), Some(needed)));
        assert!(!permitted(
            Profile::Nob,
            Some(GuildId::new(2)),
            Some(needed),
            Some(needed)
        ));
        assert!(!permitted(Profile::Nob, Some(guild), None, Some(needed)));
        assert!(!permitted(Profile::Nob, Some(guild), Some(needed), None));
        assert!(!permitted(
            Profile::Nob,
            Some(guild),
            Some(Permissions::VIEW_CHANNEL),
            Some(needed)
        ));
        assert!(!permitted(
            Profile::Nob,
            Some(guild),
            Some(needed),
            Some(Permissions::MANAGE_MESSAGES)
        ));
    }

    #[test]
    fn purge_preserves_pins_bot_messages_and_boundary_age_messages() {
        let now = 2_000_000;
        assert!(purge_eligible(false, false, now - 10, now));
        assert!(!purge_eligible(true, false, now - 10, now));
        assert!(!purge_eligible(false, true, now - 10, now));
        assert!(!purge_eligible(false, false, now - 14 * 86400 + 60, now));
        assert!(!purge_eligible(false, false, now - 15 * 86400, now));
    }
}
