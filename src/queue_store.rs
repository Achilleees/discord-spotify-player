//! Persistence for the pending queue.
//!
//! The queue survives restarts: the VPS redeploys on every push to `main`,
//! so a restart is routine rather than exceptional and losing everyone's
//! requests to one is not acceptable. There is deliberately no expiry —
//! nothing is audible unless the bot is in a voice channel with people in
//! it, so presence already gates a stale queue and elapsed time adds
//! nothing.
//!
//! The stored shape is this module's own, not [`MediaSource`]'s. Keeping a
//! separate wire format means the domain type stays free of serde and of a
//! `SpotifyUri` that cannot derive it, and a change to the in-memory type
//! cannot silently invalidate rows already on disk.

use crate::queue::{MediaSource, QueueItem};
use librespot_core::SpotifyUri;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// On-disk form of one queued item.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredSource {
    YouTube {
        url: String,
        video_id: String,
        title: String,
        channel: String,
        thumbnail_url: Option<String>,
        duration_secs: u64,
    },
    File {
        filename: String,
        attachment_url: String,
    },
    Spotify {
        /// Stored as text; `SpotifyUri` has no serde impl.
        uri: String,
        title: String,
        artist: String,
        album_art_url: Option<String>,
    },
}

impl From<&MediaSource> for StoredSource {
    fn from(s: &MediaSource) -> Self {
        match s {
            MediaSource::YouTube {
                url,
                video_id,
                title,
                channel,
                thumbnail_url,
                duration_secs,
            } => StoredSource::YouTube {
                url: url.clone(),
                video_id: video_id.clone(),
                title: title.clone(),
                channel: channel.clone(),
                thumbnail_url: thumbnail_url.clone(),
                duration_secs: *duration_secs,
            },
            MediaSource::File { filename, attachment_url } => StoredSource::File {
                filename: filename.clone(),
                attachment_url: attachment_url.clone(),
            },
            MediaSource::Spotify { uri, title, artist, album_art_url } => StoredSource::Spotify {
                uri: uri.to_string(),
                title: title.clone(),
                artist: artist.clone(),
                album_art_url: album_art_url.clone(),
            },
        }
    }
}

impl StoredSource {
    /// `None` when the row cannot be turned back into a playable item — a
    /// Spotify uri that no longer parses, say. One unreadable row is skipped
    /// rather than failing the whole restore.
    fn into_media_source(self) -> Option<MediaSource> {
        Some(match self {
            StoredSource::YouTube {
                url,
                video_id,
                title,
                channel,
                thumbnail_url,
                duration_secs,
            } => MediaSource::YouTube {
                url,
                video_id,
                title,
                channel,
                thumbnail_url,
                duration_secs,
            },
            StoredSource::File { filename, attachment_url } => {
                MediaSource::File { filename, attachment_url }
            }
            StoredSource::Spotify { uri, title, artist, album_art_url } => MediaSource::Spotify {
                uri: SpotifyUri::from_uri(&uri).ok()?,
                title,
                artist,
                album_art_url,
            },
        })
    }
}

pub struct QueueStore {
    conn: Mutex<Connection>,
}

impl QueueStore {
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queue_items (
                 position     INTEGER PRIMARY KEY,
                 source_json  TEXT NOT NULL,
                 queued_by    TEXT NOT NULL,
                 queued_by_id TEXT NOT NULL,
                 added_at     TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the stored queue with `items`, in one transaction.
    ///
    /// A whole rewrite rather than a diff: the queue is capped at a few tens
    /// of items, and reordering or removing from the middle would otherwise
    /// need position fix-ups that can leave the table disagreeing with
    /// memory. Atomicity matters more than the write volume here.
    pub fn save(&self, items: &[QueueItem]) -> rusqlite::Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM queue_items", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO queue_items (position, source_json, queued_by, queued_by_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (pos, item) in items.iter().enumerate() {
                let json = serde_json::to_string(&StoredSource::from(&item.source))
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                stmt.execute(rusqlite::params![
                    pos as i64,
                    json,
                    item.queued_by,
                    item.queued_by_id.to_string(),
                ])?;
            }
        }
        tx.commit()
    }

    /// The stored queue, in order. Rows that no longer parse are skipped and
    /// logged: a queue that restores nine of ten items still beats one that
    /// refuses to restore at all.
    pub fn load(&self) -> rusqlite::Result<Vec<QueueItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source_json, queued_by, queued_by_id
             FROM queue_items ORDER BY position ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let json: String = row.get(0)?;
                let queued_by: String = row.get(1)?;
                let queued_by_id: String = row.get(2)?;
                Ok((json, queued_by, queued_by_id))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items = Vec::with_capacity(rows.len());
        for (json, queued_by, queued_by_id) in rows {
            let Ok(stored) = serde_json::from_str::<StoredSource>(&json) else {
                tracing::warn!("skipping an unreadable queue row on restore");
                continue;
            };
            let Some(source) = stored.into_media_source() else {
                tracing::warn!("skipping a queue row whose track no longer resolves");
                continue;
            };
            let Ok(queued_by_id) = queued_by_id.parse::<u64>() else {
                tracing::warn!("skipping a queue row with an unreadable requester id");
                continue;
            };
            items.push(QueueItem::new(source, queued_by, queued_by_id));
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> QueueStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE queue_items (
                 position     INTEGER PRIMARY KEY,
                 source_json  TEXT NOT NULL,
                 queued_by    TEXT NOT NULL,
                 queued_by_id TEXT NOT NULL,
                 added_at     TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .unwrap();
        QueueStore { conn: Mutex::new(conn) }
    }

    fn yt(title: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::YouTube {
                url: format!("https://soundcloud.com/x/{title}"),
                video_id: "1".into(),
                title: title.into(),
                channel: "A Channel".into(),
                thumbnail_url: None,
                duration_secs: 271,
            },
            "Papos".into(),
            316390674608029703,
        )
    }

    fn spotify(id: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::Spotify {
                uri: SpotifyUri::from_uri(&format!("spotify:track:{id}")).unwrap(),
                title: "A Track".into(),
                artist: "An Artist".into(),
                album_art_url: None,
            },
            "pinkchaoz".into(),
            771540703075368982,
        )
    }

    #[test]
    fn round_trips_every_source_kind_in_order() {
        let s = store();
        let saved = vec![
            yt("first"),
            spotify("3iJDVtjZGLkWQOG53XdTvF"),
            QueueItem::new(
                MediaSource::File {
                    filename: "clip.mp3".into(),
                    attachment_url: "https://cdn.discord/x".into(),
                },
                "Papos".into(),
                1,
            ),
        ];
        s.save(&saved).unwrap();

        let loaded = s.load().unwrap();
        assert_eq!(loaded.len(), 3, "order and count preserved");
        assert_eq!(loaded[0].source.display_title(), "first");
        assert_eq!(loaded[1].source.display_title(), "A Track");
        assert_eq!(loaded[2].source.display_title(), "clip.mp3");
        assert_eq!(loaded[1].queued_by, "pinkchaoz");
        assert_eq!(loaded[1].queued_by_id, 771540703075368982);
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let s = store();
        s.save(&[yt("a"), yt("b")]).unwrap();
        s.save(&[yt("c")]).unwrap();

        let loaded = s.load().unwrap();
        assert_eq!(loaded.len(), 1, "the previous queue is gone, not merged");
        assert_eq!(loaded[0].source.display_title(), "c");
    }

    #[test]
    fn an_emptied_queue_persists_as_empty() {
        let s = store();
        s.save(&[yt("a")]).unwrap();
        s.save(&[]).unwrap();
        assert!(s.load().unwrap().is_empty());
    }

    #[test]
    fn one_unreadable_row_does_not_lose_the_rest() {
        let s = store();
        s.save(&[yt("good-one"), yt("good-two")]).unwrap();
        // Corrupt the middle row the way a format change would.
        {
            let conn = s.lock();
            conn.execute(
                "UPDATE queue_items SET source_json = '{\"kind\":\"nonsense\"}' WHERE position = 0",
                [],
            )
            .unwrap();
        }
        let loaded = s.load().unwrap();
        assert_eq!(loaded.len(), 1, "the readable row still restores");
        assert_eq!(loaded[0].source.display_title(), "good-two");
    }

    #[test]
    fn restoring_does_not_reuse_stored_item_ids() {
        // item_id names one residency in the queue; the queue stamps a fresh
        // one on insertion, so a restored item must not carry an old id.
        let s = store();
        s.save(&[yt("a")]).unwrap();
        assert_eq!(s.load().unwrap()[0].item_id, 0, "unstamped until pushed");
    }
}
