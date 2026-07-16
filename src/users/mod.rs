//! Per-user Spotify credential storage.
//!
//! Backed by SQLite with the same `spotify_credentials` schema nob uses, so the
//! table transplants directly. Tokens live in an encrypted `auth_blob` (see
//! [`crypto`]); the rest of the row is queryable plaintext.

mod crypto;

use crypto::TokenCipher;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// A user's stored credentials, as seen by the rest of the app.
#[derive(Clone)]
pub struct UserCredentials {
    pub discord_user_id: String,
    pub discord_name: String,
    pub spotify_username: String,
    pub access_token: String,
    pub refresh_token: String,
    pub active: bool,
}

/// Manual Debug: token fields are redacted so `{:?}` can never leak them.
impl std::fmt::Debug for UserCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserCredentials")
            .field("discord_user_id", &self.discord_user_id)
            .field("discord_name", &self.discord_name)
            .field("spotify_username", &self.spotify_username)
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("active", &self.active)
            .finish()
    }
}

/// The encrypted portion of a row.
#[derive(Serialize, Deserialize)]
struct AuthBlob {
    access_token: String,
    refresh_token: String,
}

pub struct UserStore {
    conn: Mutex<Connection>,
    cipher: TokenCipher,
}

impl UserStore {
    /// Open (creating if needed) the credential store at `db_path`, encrypting
    /// tokens with `enc_key` when present.
    pub fn open(db_path: &str, enc_key: Option<&str>) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        // The DB holds tokens (encrypted or not); keep it owner-only on unix.
        restrict_permissions(db_path);
        // last_used_at/created_at are written but never queried here: the
        // schema is intentional parity with nob's 002-music.sql (see PORT.md),
        // which does consume them.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS spotify_credentials (
                 discord_user_id  TEXT PRIMARY KEY,
                 discord_name     TEXT NOT NULL,
                 spotify_username TEXT,
                 auth_blob        BLOB,
                 is_active        INTEGER NOT NULL DEFAULT 0,
                 last_used_at     TEXT,
                 created_at       TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let store = Self {
            conn: Mutex::new(conn),
            cipher: TokenCipher::new(enc_key),
        };
        if !store.cipher.is_encrypted() {
            tracing::warn!(
                "TOKEN_ENC_KEY not set — OAuth tokens are stored unencrypted in the database"
            );
        }
        Ok(store)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read a persisted key/value setting (bot-level toggles like the
    /// /announce state, which must survive restarts).
    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .ok()
    }

    /// Persist a key/value setting.
    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    fn row_to_creds(
        &self,
        discord_user_id: String,
        discord_name: String,
        spotify_username: Option<String>,
        auth_blob: Option<Vec<u8>>,
        is_active: bool,
    ) -> Option<UserCredentials> {
        let blob = auth_blob?;
        // Distinguish a decrypt failure (wrong/rotated TOKEN_ENC_KEY, corrupt
        // blob, AAD/owner mismatch) from a genuinely absent row — otherwise it
        // looks like the user was never logged in.
        let plain = match self.cipher.open(&blob, discord_user_id.as_bytes()) {
            Some(p) => p,
            None => {
                tracing::warn!(user = %discord_user_id, "failed to decrypt stored credentials (wrong TOKEN_ENC_KEY, corrupt, or owner mismatch)");
                return None;
            }
        };
        let tokens: AuthBlob = serde_json::from_slice(&plain).ok()?;
        Some(UserCredentials {
            discord_user_id,
            discord_name,
            spotify_username: spotify_username.unwrap_or_default(),
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            active: is_active,
        })
    }

    pub fn load(&self, discord_user_id: &str) -> Option<UserCredentials> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT discord_user_id, discord_name, spotify_username, auth_blob, is_active
                 FROM spotify_credentials WHERE discord_user_id = ?1",
                [discord_user_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get::<_, i64>(4)? != 0,
                    ))
                },
            )
            .optional()
            .ok()??;
        self.row_to_creds(row.0, row.1, row.2, row.3, row.4)
    }

    pub fn save(&self, creds: &UserCredentials) -> rusqlite::Result<()> {
        let blob = self.cipher.seal(
            &serde_json::to_vec(&AuthBlob {
                access_token: creds.access_token.clone(),
                refresh_token: creds.refresh_token.clone(),
            })
            .expect("AuthBlob serializes"),
            creds.discord_user_id.as_bytes(),
        );
        let conn = self.lock();
        conn.execute(
            "INSERT INTO spotify_credentials
                 (discord_user_id, discord_name, spotify_username, auth_blob, is_active, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
             ON CONFLICT(discord_user_id) DO UPDATE SET
                 discord_name     = excluded.discord_name,
                 spotify_username = excluded.spotify_username,
                 auth_blob        = excluded.auth_blob,
                 is_active        = excluded.is_active,
                 last_used_at     = datetime('now')",
            rusqlite::params![
                creds.discord_user_id,
                creds.discord_name,
                creds.spotify_username,
                blob,
                creds.active as i64,
            ],
        )?;
        Ok(())
    }

    /// Deactivate a user. Returns `true` only when a currently-active row was
    /// flipped, so a repeat `/logout` reports "no active session".
    pub fn deactivate(&self, discord_user_id: &str) -> rusqlite::Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE spotify_credentials SET is_active = 0
             WHERE discord_user_id = ?1 AND is_active = 1",
            [discord_user_id],
        )?;
        Ok(n > 0)
    }

    /// Mark one user active and every other user inactive, atomically. Used on
    /// session takeover so at most one row is ever active.
    pub fn set_active_exclusive(&self, discord_user_id: &str) -> rusqlite::Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE spotify_credentials SET is_active = 0 WHERE discord_user_id <> ?1",
            [discord_user_id],
        )?;
        tx.execute(
            "UPDATE spotify_credentials SET is_active = 1 WHERE discord_user_id = ?1",
            [discord_user_id],
        )?;
        tx.commit()
    }

    pub fn remove(&self, discord_user_id: &str) -> rusqlite::Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM spotify_credentials WHERE discord_user_id = ?1",
            [discord_user_id],
        )?;
        Ok(n > 0)
    }

    pub fn list(&self) -> Vec<UserCredentials> {
        let conn = self.lock();
        let mut stmt = match conn.prepare(
            "SELECT discord_user_id, discord_name, spotify_username, auth_blob, is_active
             FROM spotify_credentials",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, i64>(4)? != 0,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(_) => return vec![],
        };
        rows.filter_map(|r| r.ok())
            .filter_map(|(id, name, uname, blob, active)| {
                self.row_to_creds(id, name, uname, blob, active)
            })
            .collect()
    }

}

/// Restrict the credential DB to owner-only (0600) on unix. No-op elsewhere
/// and for the in-memory (`:memory:`) test DB.
#[cfg(unix)]
fn restrict_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if path == ":memory:" {
        return;
    }
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %e, "failed to set 0600 on credential DB");
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UserStore {
        // A unique in-memory DB per call; no key so we exercise the plaintext path.
        UserStore::open(":memory:", None).unwrap()
    }

    fn creds(id: &str, active: bool) -> UserCredentials {
        UserCredentials {
            discord_user_id: id.to_string(),
            discord_name: "Achille".to_string(),
            spotify_username: "achille_sp".to_string(),
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            active,
        }
    }

    #[test]
    fn save_then_load_roundtrips_tokens() {
        let s = store();
        s.save(&creds("1", true)).unwrap();
        let got = s.load("1").unwrap();
        assert_eq!(got.access_token, "at");
        assert_eq!(got.refresh_token, "rt");
        assert!(got.active);
    }

    #[test]
    fn save_upserts_not_duplicates() {
        let s = store();
        s.save(&creds("1", true)).unwrap();
        let mut c = creds("1", true);
        c.access_token = "at2".to_string();
        s.save(&c).unwrap();
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.load("1").unwrap().access_token, "at2");
    }

    #[test]
    fn deactivate_reports_only_a_real_transition() {
        let s = store();
        s.save(&creds("1", true)).unwrap();
        assert!(s.deactivate("1").unwrap(), "first logout flips active");
        assert!(!s.deactivate("1").unwrap(), "second logout is a no-op");
    }

    #[test]
    fn set_active_exclusive_leaves_one_active() {
        let s = store();
        s.save(&creds("1", true)).unwrap();
        s.save(&creds("2", true)).unwrap();
        s.set_active_exclusive("2").unwrap();
        let active: Vec<_> = s.list().into_iter().filter(|u| u.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].discord_user_id, "2");
    }

    #[test]
    fn remove_deletes() {
        let s = store();
        s.save(&creds("1", true)).unwrap();
        assert!(s.remove("1").unwrap());
        assert!(s.load("1").is_none());
        assert!(!s.remove("1").unwrap());
    }

    #[test]
    fn tokens_are_encrypted_at_rest_with_a_key() {
        let s = UserStore::open(":memory:", Some("a-key")).unwrap();
        s.save(&creds("1", true)).unwrap();
        // Read the raw blob and confirm the token text isn't present.
        let blob: Vec<u8> = s
            .lock()
            .query_row(
                "SELECT auth_blob FROM spotify_credentials WHERE discord_user_id='1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!blob.windows(2).any(|w| w == b"at"));
        // But the store still decrypts it.
        assert_eq!(s.load("1").unwrap().access_token, "at");
    }
}
