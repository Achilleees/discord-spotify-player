use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

const CREDS_DIR: &str = ".user_creds";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCredentials {
    pub discord_user_id: String,
    pub spotify_username: String,
    pub access_token: String,
    pub refresh_token: String,
    pub paired_at: String,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

pub struct UserStore {
    base_dir: PathBuf,
}

impl UserStore {
    pub fn new() -> Self {
        Self {
            base_dir: PathBuf::from(CREDS_DIR),
        }
    }

    fn cred_path(&self, discord_user_id: &str) -> PathBuf {
        self.base_dir.join(format!("{}.json", discord_user_id))
    }

    pub fn save(&self, creds: &UserCredentials) -> io::Result<()> {
        std::fs::create_dir_all(&self.base_dir)?;
        let path = self.cred_path(&creds.discord_user_id);
        let json = serde_json::to_string_pretty(creds)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }

    pub fn load(&self, discord_user_id: &str) -> Option<UserCredentials> {
        let path = self.cred_path(discord_user_id);
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Soft-deactivate: keeps credentials but marks active = false.
    pub fn deactivate(&self, discord_user_id: &str) -> io::Result<bool> {
        match self.load(discord_user_id) {
            Some(mut creds) => {
                creds.active = false;
                self.save(&creds)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Hard-delete: removes the credentials file entirely.
    pub fn remove(&self, discord_user_id: &str) -> io::Result<bool> {
        let path = self.cred_path(discord_user_id);
        if path.exists() {
            std::fs::remove_file(path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn list(&self) -> Vec<UserCredentials> {
        let dir = match std::fs::read_dir(&self.base_dir) {
            Ok(d) => d,
            Err(_) => return vec![],
        };
        dir.filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let data = std::fs::read_to_string(&path).ok()?;
                serde_json::from_str(&data).ok()
            } else {
                None
            }
        })
        .collect()
    }
}
