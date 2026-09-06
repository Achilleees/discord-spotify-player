//! Process-level identity, configuration and exclusive writable resources.
//! Each host calls the runtime once. Frozen paths are shared by its media
//! helpers; no helper consults another identity's environment at playback time.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Spotibot,
    Nob,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Spotibot => "Spotibot",
            Self::Nob => "nob",
        }
    }

    pub fn env_file(self) -> &'static str {
        match self {
            Self::Spotibot => ".env",
            Self::Nob => ".env.nob",
        }
    }
}

#[derive(Default)]
pub(crate) struct Options {
    pub env_file: Option<PathBuf>,
    pub check: bool,
    pub setup: bool,
    pub help: bool,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> io::Result<Self> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--env-file" if result.env_file.is_none() => {
                    let path = args
                        .next()
                        .filter(|v| !v.is_empty() && !v.starts_with('-'))
                        .ok_or_else(|| io::Error::other("--env-file needs a path"))?;
                    result.env_file = Some(path.into());
                }
                "--check-config" => result.check = true,
                "--setup" => result.setup = true,
                "--help" | "-h" => result.help = true,
                _ => return Err(io::Error::other("unknown or repeated argument; use --help")),
            }
        }
        if result.setup && (result.check || result.env_file.is_some()) {
            return Err(io::Error::other(
                "--setup cannot be combined with --env-file or --check-config",
            ));
        }
        Ok(result)
    }
}

// Deliberately no Debug: values include credentials.
pub(crate) struct Settings {
    pub profile: Profile,
    values: HashMap<String, String>,
}

impl Settings {
    pub fn load(profile: Profile, file: Option<&Path>) -> io::Result<Self> {
        let path = file.unwrap_or_else(|| Path::new(profile.env_file()));
        let mut values = HashMap::new();
        match dotenvy::from_path_iter(path) {
            Ok(entries) => {
                for entry in entries {
                    // dotenv parse errors may contain the original credential line.
                    let (key, value) =
                        entry.map_err(|_| io::Error::other("invalid env file syntax"))?;
                    values.insert(key, value);
                }
            }
            Err(dotenvy::Error::Io(e)) if e.kind() == io::ErrorKind::NotFound && file.is_none() => {
            }
            Err(_) => return Err(io::Error::other("could not read env file")),
        }
        Ok(Self::merge(
            profile,
            values,
            std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            }),
        ))
    }

    fn merge(
        profile: Profile,
        mut values: HashMap<String, String>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        for (key, value) in env {
            match profile {
                Profile::Spotibot => {
                    values.insert(key, value);
                }
                Profile::Nob => {
                    // No fallback to unprefixed process variables, even when the
                    // nob credential is absent. Both hosts can use the same cwd.
                    if let Some(key) = key.strip_prefix("NOB_") {
                        values.insert(key.to_owned(), value);
                    }
                }
            }
        }
        Self { profile, values }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

#[derive(Clone)]
pub(crate) struct Paths {
    pub database: PathBuf,
    pub spotify_cache: PathBuf,
    pub youtube_tmp: PathBuf,
    pub youtube_cookies: PathBuf,
    pub dj_clips: PathBuf,
    pub dj_cache: PathBuf,
    #[cfg(unix)]
    pub kokoro_socket: PathBuf,
    pub youtube_max_duration: Option<String>,
}

impl Paths {
    pub fn resolve(settings: &Settings, cwd: &Path) -> io::Result<Self> {
        let explicit_state = settings.get("STATE_DIR");
        let legacy = settings.profile == Profile::Spotibot && explicit_state.is_none();
        let state = explicit_state.unwrap_or(if legacy { "." } else { ".nob" });
        if state.trim().is_empty() {
            return Err(io::Error::other("STATE_DIR cannot be blank"));
        }
        let state = cwd.join(state);
        let resolve = |key, default: &str| -> io::Result<PathBuf> {
            let value = settings.get(key).unwrap_or(default);
            if value.trim().is_empty() {
                return Err(io::Error::other(format!("{key} cannot be blank")));
            }
            Ok(state.join(value))
        };
        let database = match (settings.get("DATABASE_PATH"), settings.profile) {
            (None, Profile::Spotibot) if settings.get("SPOTIBOT_DB").is_some() => {
                let value = settings.get("SPOTIBOT_DB").unwrap();
                if value.trim().is_empty() {
                    return Err(io::Error::other("SPOTIBOT_DB cannot be blank"));
                }
                // Legacy override has always been relative to the process cwd.
                cwd.join(value)
            }
            _ => resolve(
                "DATABASE_PATH",
                if settings.profile == Profile::Nob {
                    "nob.db"
                } else {
                    "spotibot.db"
                },
            )?,
        };
        Ok(Self {
            database,
            spotify_cache: resolve("SPOTIFY_CACHE_DIR", ".spotify_cache")?,
            youtube_tmp: resolve(
                "YOUTUBE_TMP_DIR",
                if legacy {
                    "/tmp/spotibot-youtube"
                } else {
                    "youtube-tmp"
                },
            )?,
            youtube_cookies: resolve(
                "YOUTUBE_COOKIES",
                if legacy {
                    "/var/lib/spotibot/youtube-cookies.txt"
                } else {
                    "youtube-cookies.txt"
                },
            )?,
            dj_clips: resolve(
                "DJ_CLIPS_DIR",
                if legacy {
                    "/var/lib/spotibot/dj-clips"
                } else {
                    "dj-clips"
                },
            )?,
            dj_cache: resolve(
                "DJ_CACHE_DIR",
                if legacy {
                    "/var/lib/spotibot/dj-cache"
                } else {
                    "dj-cache"
                },
            )?,
            #[cfg(unix)]
            kokoro_socket: resolve(
                "KOKORO_SOCKET",
                if legacy {
                    "/var/lib/spotibot/kokoro.sock"
                } else {
                    "kokoro.sock"
                },
            )?,
            youtube_max_duration: settings.get("YOUTUBE_MAX_DURATION_SECS").map(str::to_owned),
        })
    }

    pub fn install(self) -> io::Result<()> {
        PATHS.set(self).map_err(|_| {
            io::Error::other("runtime already initialized; run each bot in its own process")
        })
    }

    pub fn lock(&self) -> io::Result<StateLocks> {
        let mut files = Vec::new();
        for dir in [&self.spotify_cache, &self.youtube_tmp, &self.dj_cache] {
            fs::create_dir_all(dir)?;
            files.push(fs::canonicalize(dir)?.join(".bot.lock"));
        }
        // yt-dlp writes its cookie jar back, so it is writable state too.
        for path in [&self.database, &self.youtube_cookies] {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("state file needs a parent directory"))?;
            fs::create_dir_all(parent)?;
            let canonical = if path.exists() {
                fs::canonicalize(path)?
            } else {
                fs::canonicalize(parent)?.join(
                    path.file_name()
                        .ok_or_else(|| io::Error::other("state file needs a filename"))?,
                )
            };
            let mut lock = canonical.into_os_string();
            lock.push(".bot.lock");
            files.push(lock.into());
        }
        StateLocks::acquire(files)
    }
}

static PATHS: OnceLock<Paths> = OnceLock::new();

pub(crate) fn paths() -> &'static Paths {
    PATHS
        .get()
        .expect("paths installed before starting runtime helpers")
}

// Keep the open handles alive for the entire process lifetime. Never unlink a
// lock file: another process may already have opened the same inode for locking.
pub(crate) struct StateLocks {
    _files: Vec<File>,
}

impl StateLocks {
    fn acquire(paths: Vec<PathBuf>) -> io::Result<Self> {
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        for path in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            file.try_lock().map_err(|_| io::Error::other("bot state is already in use or cannot be locked; choose separate database and cache paths"))?;
            files.push(file);
        }
        Ok(Self { _files: files })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(profile: Profile, pairs: &[(&str, &str)]) -> Settings {
        Settings::merge(
            profile,
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            [],
        )
    }

    #[test]
    fn nob_never_inherits_spotibot_identity_or_paths() {
        let env = [
            ("DISCORD_TOKEN", "spotibot-test"),
            ("STATE_DIR", "shared"),
            ("DEVICE_ID", "shared"),
            ("NOB_DEVICE_NAME", "My nob"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let config = Settings::merge(Profile::Nob, HashMap::new(), env);
        assert_eq!(config.get("DISCORD_TOKEN"), None);
        assert_eq!(config.get("STATE_DIR"), None);
        assert_eq!(config.get("DEVICE_ID"), None);
        assert_eq!(config.get("DEVICE_NAME"), Some("My nob"));
    }

    #[test]
    fn scoped_environment_wins_over_the_selected_file() {
        let env = [("NOB_DEVICE_NAME".into(), "override".into())];
        let config = Settings::merge(
            Profile::Nob,
            HashMap::from([("DEVICE_NAME".into(), "file".into())]),
            env,
        );
        assert_eq!(config.get("DEVICE_NAME"), Some("override"));
        let config = Settings::merge(
            Profile::Spotibot,
            HashMap::from([("DEVICE_NAME".into(), "file".into())]),
            [("DEVICE_NAME".into(), "legacy".into())],
        );
        assert_eq!(config.get("DEVICE_NAME"), Some("legacy"));
    }

    #[test]
    fn hosts_have_disjoint_default_writable_paths() {
        let cwd = std::env::current_dir().unwrap();
        let spot = Paths::resolve(&settings(Profile::Spotibot, &[]), &cwd).unwrap();
        let nob = Paths::resolve(&settings(Profile::Nob, &[]), &cwd).unwrap();
        assert_eq!(spot.database, cwd.join("./spotibot.db"));
        for path in [
            &nob.database,
            &nob.spotify_cache,
            &nob.youtube_tmp,
            &nob.dj_cache,
            &nob.youtube_cookies,
        ] {
            assert!(path.starts_with(cwd.join(".nob")));
        }
        assert_ne!(spot.database, nob.database);
        assert_ne!(spot.spotify_cache, nob.spotify_cache);
        assert_ne!(spot.youtube_tmp, nob.youtube_tmp);
        assert_ne!(spot.dj_cache, nob.dj_cache);
    }

    #[test]
    fn explicit_state_contains_relative_paths_and_retains_absolute_overrides() {
        let cwd = std::env::current_dir().unwrap();
        let paths = Paths::resolve(
            &settings(
                Profile::Spotibot,
                &[("STATE_DIR", "instance"), ("DATABASE_PATH", "custom.db")],
            ),
            &cwd,
        )
        .unwrap();
        assert_eq!(paths.database, cwd.join("instance/custom.db"));
        assert_eq!(paths.youtube_tmp, cwd.join("instance/youtube-tmp"));
        let absolute = cwd.join("external-cache");
        let paths = Paths::resolve(
            &settings(
                Profile::Nob,
                &[("DJ_CACHE_DIR", absolute.to_str().unwrap())],
            ),
            &cwd,
        )
        .unwrap();
        assert_eq!(paths.dj_cache, absolute);
        assert!(Paths::resolve(&settings(Profile::Nob, &[("STATE_DIR", " ")]), &cwd).is_err());
    }

    #[test]
    fn state_locks_exclude_shared_resources_and_release_on_drop() {
        let root = std::env::temp_dir().join(format!("bot-lock-test-{}", uuid::Uuid::new_v4()));
        let first = Paths::resolve(&settings(Profile::Nob, &[]), &root.join("first")).unwrap();
        let second = Paths::resolve(&settings(Profile::Nob, &[]), &root.join("second")).unwrap();
        let held = first.lock().unwrap();
        let independent = second.lock().unwrap();
        assert!(first.lock().is_err());
        drop(independent);
        for index in 0..5 {
            let mut overlap = second.clone();
            match index {
                0 => overlap.database = first.database.clone(),
                1 => overlap.spotify_cache = first.spotify_cache.clone(),
                2 => overlap.youtube_tmp = first.youtube_tmp.clone(),
                3 => overlap.dj_cache = first.dj_cache.clone(),
                _ => overlap.youtube_cookies = first.youtube_cookies.clone(),
            }
            assert!(overlap.lock().is_err());
        }
        drop(held);
        drop(first.lock().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_rejects_ambiguous_or_unsafe_setup_combinations() {
        let parse = |args: &[&str]| Options::parse(args.iter().map(|s| s.to_string()));
        assert!(parse(&["--env-file"]).is_err());
        assert!(parse(&["--env-file", "--check-config"]).is_err());
        assert!(parse(&["--setup", "--check-config"]).is_err());
        assert!(parse(&["--setup", "--env-file", "custom.env"]).is_err());
        assert!(parse(&["--unknown"]).is_err());
        assert!(
            parse(&["--env-file", "config.env", "--check-config"])
                .unwrap()
                .check
        );
    }
}
