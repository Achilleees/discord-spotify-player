//! What actually aired, in order.
//!
//! Append-only: a row is written when a track *starts playing*, never when
//! it is queued. Spotify's own playlist and autoplay tracks are recorded the
//! same way as requests — the bot drives the account, so this log, not
//! Spotify's, is the record of what the room heard.
//!
//! "Starts playing" is the moment the bot commits to it, which for a queue
//! item is slightly ahead of the first sample: a track skipped a second in
//! still leaves a row. That is deliberate — the alternative is recording at
//! the end, which loses everything skipped and everything still playing.
//!
//! Each row carries the `context_uri` it aired from. That is what lets a
//! later back-jump reopen the playlist positioned at a track
//! (`LoadRequest::from_context_uri`) rather than replacing it with a
//! one-track context.

use crate::player::state::{AiredSource, AiredTrack};
use rusqlite::{Connection, OptionalExtension};
use std::sync::Mutex;

/// One aired row, as read back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub id: i64,
    /// Unix seconds, so the listing can hand Discord a timestamp it renders
    /// in each reader's own timezone. Stored as text; converted on read.
    pub aired_at_unix: i64,
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
                 -- Written but never read back: the listing shows a plain
                 -- name rather than a mention, because a history listing
                 -- that pings everyone in it is a bad listing. Kept because
                 -- it is the durable identity behind the name, and it
                 -- cannot be recovered later if it isn't stored now.
                 queued_by_id TEXT
             );
             -- Both reads order and filter by id, not aired_at, so an
             -- aired_at index would be write amplification on the hot
             -- insert path and nothing else. id is already the primary key.
             ",
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

    /// The row aired immediately before `before`, or the one before the
    /// newest row when `before` is `None` (i.e. "the track before whatever
    /// is playing"). `None` means there is nothing further back.
    ///
    /// Back-navigation walks by row id rather than re-reading "the
    /// second-newest row" each time: replaying a track appends a row of its
    /// own, so the naive query would bounce between two tracks forever.
    pub fn aired_before(&self, before: Option<i64>) -> rusqlite::Result<Option<HistoryRow>> {
        let conn = self.lock();
        let anchor: i64 = match before {
            Some(id) => id,
            None => {
                match conn
                    .query_row("SELECT MAX(id) FROM play_history", [], |r| {
                        r.get::<_, Option<i64>>(0)
                    })? {
                    Some(newest) => newest,
                    None => return Ok(None),
                }
            }
        };
        conn.query_row(
            "SELECT id, CAST(strftime('%s', aired_at) AS INTEGER), source, track_ref,
                    context_uri, title, artist, queued_by
             FROM play_history WHERE id < ?1 ORDER BY id DESC LIMIT 1",
            [anchor],
            |row| {
                let source: String = row.get(2)?;
                Ok(HistoryRow {
                    id: row.get(0)?,
                    aired_at_unix: row.get(1)?,
                    source: AiredSource::from_str(&source),
                    track_ref: row.get(3)?,
                    context_uri: row.get(4)?,
                    title: row.get(5)?,
                    artist: row.get(6)?,
                    queued_by: row.get(7)?,
                })
            },
        )
        .optional()
    }

    /// The most recently aired tracks, newest first.
    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<HistoryRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, CAST(strftime('%s', aired_at) AS INTEGER), source, track_ref,
                    context_uri, title, artist, queued_by
             FROM play_history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                let source: String = row.get(2)?;
                Ok(HistoryRow {
                    id: row.get(0)?,
                    aired_at_unix: row.get(1)?,
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

    /// Built through `open` itself, against a real (temporary) file, so the
    /// schema under test is the one that ships. A hand-copied `CREATE TABLE`
    /// here would let a renamed column pass every test and fail on the live
    /// database.
    static NEXT_DB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn store() -> HistoryStore {
        let path = std::env::temp_dir().join(format!(
            "spotibot-history-test-{}-{}.db",
            std::process::id(),
            // A counter, not a thread id: ids get reused, and on Windows
            // removing a file another test still holds open fails silently
            // — which would leak one test's rows into the next.
            NEXT_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        HistoryStore::open(path.to_str().unwrap()).unwrap()
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
    fn walking_back_by_id_does_not_bounce_between_two_tracks() {
        // The whole reason back-navigation carries a cursor: replaying a
        // track appends a row, so "the second-newest row" would alternate
        // between the last two tracks forever.
        let s = store();
        for t in ["one", "two", "three"] {
            s.record(&baseline(t)).unwrap();
        }

        // From live: the track before whatever is playing ("three").
        let first_back = s.aired_before(None).unwrap().unwrap();
        assert_eq!(first_back.track_ref, "two");

        // Replaying it appends a row — and the next step back must still go
        // to "one", not back to "two".
        s.record(&baseline("two")).unwrap();
        let second_back = s.aired_before(Some(first_back.id)).unwrap().unwrap();
        assert_eq!(second_back.track_ref, "one");
    }

    #[test]
    fn walking_past_the_start_reports_nothing_rather_than_wrapping() {
        let s = store();
        s.record(&baseline("only")).unwrap();
        // Nothing aired before the single row.
        let oldest = s.recent(1).unwrap()[0].id;
        assert_eq!(s.aired_before(Some(oldest)).unwrap(), None);
    }

    #[test]
    fn walking_back_on_an_empty_history_is_not_an_error() {
        let s = store();
        assert_eq!(s.aired_before(None).unwrap(), None);
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
