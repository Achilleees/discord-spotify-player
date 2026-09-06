//! Typed requests between independent bot processes. Discord interactions stay
//! with their application; each performer owns authorization and playback.

pub(crate) mod transport;

use crate::config::ConfigError;
use crate::runtime::{Profile, Settings};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CommandMode {
    #[default]
    Standalone,
    Coordinator,
    Worker,
}

#[derive(Clone, Default)]
pub(crate) struct RoutingConfig {
    pub mode: CommandMode,
    pub listen: Option<SocketAddr>,
    pub peer: Option<SocketAddr>,
    pub key: Option<[u8; 32]>,
}

impl RoutingConfig {
    pub fn load(settings: &Settings) -> Result<Self, ConfigError> {
        let mode = match settings.get("COMMAND_MODE").unwrap_or("standalone") {
            "standalone" => CommandMode::Standalone,
            "coordinator" if settings.profile == Profile::Nob => CommandMode::Coordinator,
            "worker" if settings.profile == Profile::Spotibot => CommandMode::Worker,
            _ => return Err(ConfigError::Invalid("COMMAND_MODE")),
        };
        let address = |name| -> Result<Option<SocketAddr>, ConfigError> {
            let Some(raw) = settings.get(name).filter(|v| !v.trim().is_empty()) else {
                return Ok(None);
            };
            let addr: SocketAddr = raw.parse().map_err(|_| ConfigError::Invalid(name))?;
            if !addr.ip().is_loopback() || addr.port() == 0 {
                return Err(ConfigError::Invalid(name));
            }
            Ok(Some(addr))
        };
        let listen = address("ROUTING_LISTEN")?;
        let peer = address("ROUTING_PEER")?;
        let key = settings
            .get("ROUTING_KEY")
            .map(|raw| {
                let bytes = hex::decode(raw).map_err(|_| ConfigError::Invalid("ROUTING_KEY"))?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| ConfigError::Invalid("ROUTING_KEY"))?;
                if key == [0; 32] {
                    return Err(ConfigError::Invalid("ROUTING_KEY"));
                }
                Ok(key)
            })
            .transpose()?;
        if mode != CommandMode::Standalone && (listen.is_none() || key.is_none()) {
            return Err(ConfigError::Missing("ROUTING_LISTEN / ROUTING_KEY"));
        }
        if mode == CommandMode::Coordinator && (peer.is_none() || peer == listen) {
            return Err(ConfigError::Invalid("ROUTING_PEER"));
        }
        Ok(Self {
            mode,
            listen,
            peer,
            key,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Target {
    pub boot: String,
    pub generation: u64,
    pub room: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Status {
    pub target: Target,
    pub name: String,
    pub guild: u64,
    pub bot: u64,
    pub ready: bool,
    pub media: bool,
    pub now: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum Control {
    Play,
    Previous,
    Skip,
    Pause,
    Stop,
    Clear,
    Announce,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum View {
    Now,
    Queue,
    History,
    Account,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Attachment {
    pub filename: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Action {
    Status,
    Result {
        request: String,
    },
    View(View),
    Control(Control),
    Play {
        input: Option<String>,
        attachment: Option<Attachment>,
        next: bool,
        queued: bool,
    },
    Search {
        query: String,
        next: bool,
        queued: bool,
    },
    Login,
    FinishLogin {
        pairing: String,
    },
    Logout,
    Forget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Request {
    pub id: String,
    pub expires: u64,
    pub guild: u64,
    pub user: u64,
    pub room: Option<u64>,
    pub target: Option<Target>,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SearchHit {
    pub title: String,
    pub detail: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Reply {
    Status(Status),
    Text(String),
    Search(Vec<SearchHit>),
    Pairing {
        url: String,
        code: String,
        pairing: String,
    },
    Error(String),
    Pending,
}

/// Select only from fresh status reports. Unknown peer status is handled by the
/// caller as an explicit picker, never guessed to mean idle. A paused connection
/// remains owned. Transport actions cannot recruit a second player.
pub(crate) fn choose(
    room: Option<u64>,
    bots: &[Status],
    may_summon: bool,
    needs_media: bool,
) -> Result<usize, &'static str> {
    let room = room.ok_or("Join a voice channel, or choose a bot to inspect its music panel.")?;
    let serving: Vec<_> = bots
        .iter()
        .enumerate()
        .filter(|(_, b)| b.ready && b.target.room == Some(room))
        .collect();
    match serving.as_slice() {
        [(i, _)] => return Ok(*i),
        [] => {}
        _ => return Err("Both bots are in your room. Choose which one to control."),
    }
    if !may_summon {
        return Err("No music bot is serving your voice room.");
    }
    bots.iter()
        .position(|bot| bot.ready && bot.target.room.is_none() && (!needs_media || bot.media))
        .ok_or("Both bots are busy or unavailable. Their other rooms will keep playing.")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bot(room: Option<u64>) -> Status {
        Status {
            target: Target {
                boot: "test".into(),
                generation: 1,
                room,
            },
            name: "bot".into(),
            guild: 1,
            bot: 2,
            ready: true,
            media: true,
            now: String::new(),
        }
    }
    #[test]
    fn existing_room_beats_free_preferred_bot() {
        assert_eq!(
            choose(Some(10), &[bot(None), bot(Some(10))], true, false),
            Ok(1)
        );
    }
    #[test]
    fn free_preferred_then_free_second_but_never_another_room() {
        assert_eq!(
            choose(Some(10), &[bot(None), bot(None)], true, false),
            Ok(0)
        );
        assert_eq!(
            choose(Some(10), &[bot(Some(20)), bot(None)], true, false),
            Ok(1)
        );
        assert!(choose(Some(10), &[bot(Some(20)), bot(Some(30))], true, false).is_err());
    }
    #[test]
    fn transport_and_ambiguity_require_a_real_choice() {
        assert!(choose(Some(10), &[bot(None), bot(None)], false, false).is_err());
        assert!(choose(Some(10), &[bot(Some(10)), bot(Some(10))], false, false).is_err());
        assert!(choose(None, &[bot(None)], true, false).is_err());
    }
    #[test]
    fn only_free_capable_bots_are_recruited() {
        let mut spotify_only = bot(None);
        spotify_only.media = false;
        assert_eq!(
            choose(Some(10), &[spotify_only.clone(), bot(None)], true, true),
            Ok(1)
        );
        spotify_only.target.room = Some(10);
        // An existing session remains the user's target; unsupported requests
        // explain the missing capability instead of adding another bot there.
        assert_eq!(
            choose(Some(10), &[spotify_only, bot(None)], true, true),
            Ok(0)
        );
    }
}
