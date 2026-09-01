//! What actually aired, in order.
//!
//! Append-only: a row is written when a track *becomes audible*, never when
//! it is queued. Spotify's own playlist and autoplay tracks are recorded the
//! same way as requests — the bot drives the account, so this log, not
//! Spotify's, is the record of what the room heard.
//!
//! Each row carries the `context_uri` it aired from. That is what lets a
//! later back-jump reopen the playlist positioned at a track
//! (`LoadRequest::from_context_uri`) rather than replacing it with a
//! one-track context.

#[cfg(test)]
use crate::player::state::AiredSource;
use crate::player::state::AiredTrack;
use rusqlite::Connection;
use std::sync::Mutex;

/// One aired row, as read back out. Test-only until something reads the
/// history back (the `/history` listing and back-navigation both will).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub id: i64,
    pub aired_at: String,
    pub source: AiredSource,
    pub track_ref: String,
    pub context_uri: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub queued_by: Option<String>,
}

pub struct HistoryStore {
    conn: Mutex<Connection>,
}

impl HistoryStore {
    /// Open (creating if needed) the play-history table in the same database
    /// the credential store uses. WAL is already set there; opening a second
    /// connection to the same file is safe under it.
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS play_history (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 aired_at     TEXT NOT NULL DEFAULT (datetime('now')),
                 source       TEXT NOT NULL,
                 track_ref    TEXT NOT NULL,
                 context_uri  TEXT,
                 title        TEXT,
                 artist       TEXT,
                 queued_by    TEXT,
                 queued_by_id TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_play_history_aired_at
                 ON play_history (aired_at);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append one aired track. Called from a spawned task — the player actor
    /// never blocks on the database.
    pub fn record(&self, t: &AiredTrack) -> rusqlite::Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO play_history
                 (source, track_ref, context_uri, title, artist, queued_by, queued_by_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                t.source.as_str(),
                t.track_ref,
                t.context_uri,
                t.title,
                t.artist,
                t.queued_by,
                t.queued_by_id,
            ],
        )?;
        Ok(())
    }

    /// The most recently aired tracks, newest first.
    #[cfg(test)]
    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<HistoryRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, aired_at, source, track_ref, context_uri, title, artist, queued_by
             FROM play_history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                let source: String = row.get(2)?;
                Ok(HistoryRow {
                    id: row.get(0)?,
                    aired_at: row.get(1)?,
                    source: AiredSource::from_str(&source),
                    track_ref: row.get(3)?,
                    context_uri: row.get(4)?,
                    title: row.get(5)?,
                    artist: row.get(6)?,
                    queued_by: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> HistoryStore {
        // Each test gets its own in-memory database.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE play_history (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 aired_at     TEXT NOT NULL DEFAULT (datetime('now')),
                 source       TEXT NOT NULL,
                 track_ref    TEXT NOT NULL,
                 context_uri  TEXT,
                 title        TEXT,
                 artist       TEXT,
                 queued_by    TEXT,
                 queued_by_id TEXT
             );",
        )
        .unwrap();
        HistoryStore { conn: Mutex::new(conn) }
    }

    fn baseline(track: &str) -> AiredTrack {
        AiredTrack {
            source: AiredSource::Baseline,
            track_ref: track.into(),
            context_uri: Some("spotify:playlist:abc".into()),
            title: Some("A Title".into()),
            artist: Some("An Artist".into()),
            queued_by: None,
            queued_by_id: None,
        }
    }

    #[test]
    fn records_and_reads_back_newest_first() {
        let s = store();
        s.record(&baseline("spotify:track:one")).unwrap();
        s.record(&baseline("spotify:track:two")).unwrap();

        let rows = s.recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].track_ref, "spotify:track:two", "newest first");
        assert_eq!(rows[1].track_ref, "spotify:track:one");
    }

    #[test]
    fn keeps_the_context_so_a_back_jump_can_reopen_it() {
        let s = store();
        s.record(&baseline("spotify:track:one")).unwrap();
        let rows = s.recent(1).unwrap();
        assert_eq!(rows[0].context_uri.as_deref(), Some("spotify:playlist:abc"));
        assert_eq!(rows[0].source, AiredSource::Baseline);
    }

    #[test]
    fn a_request_keeps_who_asked_for_it() {
        let s = store();
        s.record(&AiredTrack {
            source: AiredSource::Request,
            track_ref: "https://soundcloud.com/x/y".into(),
            context_uri: None,
            title: Some("A Track".into()),
            artist: Some("A Channel".into()),
            queued_by: Some("Papos".into()),
            queued_by_id: Some("316390674608029703".into()),
        })
        .unwrap();

        let rows = s.recent(1).unwrap();
        assert_eq!(rows[0].source, AiredSource::Request);
        assert_eq!(rows[0].queued_by.as_deref(), Some("Papos"));
        // A media request has no Spotify context to reopen.
        assert_eq!(rows[0].context_uri, None);
    }

    #[test]
    fn the_same_track_airing_twice_is_two_rows() {
        // History is a log, not a set: replays and repeats both count.
        let s = store();
        s.record(&baseline("spotify:track:one")).unwrap();
        s.record(&baseline("spotify:track:one")).unwrap();
        assert_eq!(s.recent(10).unwrap().len(), 2);
    }

    #[test]
    fn recent_respects_its_limit() {
        let s = store();
        for i in 0..5 {
            s.record(&baseline(&format!("spotify:track:{i}"))).unwrap();
        }
        assert_eq!(s.recent(2).unwrap().len(), 2);
    }
}
