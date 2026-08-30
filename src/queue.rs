use std::collections::VecDeque;

use librespot_core::SpotifyUri;

#[derive(Clone, Debug)]
pub enum MediaSource {
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
        uri: SpotifyUri,
        title: String,
        artist: String,
        album_art_url: Option<String>,
    },
}

impl MediaSource {
    pub fn display_title(&self) -> &str {
        match self {
            MediaSource::YouTube { title, .. } => title,
            MediaSource::File { filename, .. } => filename,
            MediaSource::Spotify { title, .. } => title,
        }
    }

    pub fn display_subtitle(&self) -> String {
        match self {
            MediaSource::YouTube { channel, .. } => channel.clone(),
            MediaSource::File { .. } => "File upload".to_string(),
            MediaSource::Spotify { artist, .. } => artist.clone(),
        }
    }

    /// Track length as "M:SS" (or "H:MM:SS"); None for file uploads, whose
    /// length isn't known until decode, and for Spotify tracks, whose
    /// duration isn't fetched by the queue layer.
    pub fn display_duration(&self) -> Option<String> {
        match self {
            MediaSource::YouTube { duration_secs, .. } => {
                let h = duration_secs / 3600;
                let m = (duration_secs % 3600) / 60;
                let s = duration_secs % 60;
                Some(if h > 0 {
                    format!("{h}:{m:02}:{s:02}")
                } else {
                    format!("{m}:{s:02}")
                })
            }
            MediaSource::File { .. } => None,
            MediaSource::Spotify { .. } => None,
        }
    }

    pub fn embed_color(&self) -> u32 {
        match self {
            MediaSource::YouTube { .. } => 0xFF0000,
            MediaSource::File { .. } => 0x5865F2,
            MediaSource::Spotify { .. } => 0x1DB954,
        }
    }
}

#[derive(Clone, Debug)]
pub struct QueueItem {
    pub source: MediaSource,
    pub queued_by: String,
    pub queued_by_id: u64,
    /// Queue-assigned identity, stamped by `push`/`push_front`/`insert` from
    /// the queue's monotonic counter. `0` means "not yet queued"; every
    /// insertion (including a re-insertion of a popped item) stamps a fresh
    /// id, so an id names one residency in the queue, not the track itself.
    pub item_id: u64,
}

impl QueueItem {
    /// Build an item awaiting insertion; the queue stamps `item_id` when the
    /// item is pushed, so constructors never invent ids.
    // Unused until the C3 call-site migration; bot.rs's struct literals
    // predate this constructor.
    #[allow(dead_code)]
    pub fn new(source: MediaSource, queued_by: String, queued_by_id: u64) -> Self {
        Self { source, queued_by, queued_by_id, item_id: 0 }
    }
}

/// Maximum queued items (matches nob's unified-queue cap).
pub const MAX_QUEUE_LEN: usize = 500;

/// Bot-managed priority queue for YouTube/file tracks.
pub struct PriorityQueue {
    items: VecDeque<QueueItem>,
    /// Monotonic `item_id` source; the last stamped id (0 = none yet).
    /// Never reused, so ids stay unique across pops and reorders.
    next_item_id: u64,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self { items: VecDeque::new(), next_item_id: 0 }
    }

    /// Stamp the next monotonic id onto an item about to be inserted.
    fn stamp(&mut self, item: &mut QueueItem) {
        self.next_item_id += 1;
        item.item_id = self.next_item_id;
    }

    /// Enqueue an item. Returns `false` (rejecting it) when the queue is full,
    /// so an in-voice user can't grow it without bound.
    pub fn push(&mut self, mut item: QueueItem) -> bool {
        if self.items.len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.stamp(&mut item);
        self.items.push_back(item);
        true
    }

    /// Enqueue an item at the front, ahead of everything already queued.
    /// Returns `false` (rejecting it) when the queue is full, matching
    /// `push`'s cap semantics.
    pub fn push_front(&mut self, mut item: QueueItem) -> bool {
        if self.items.len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.stamp(&mut item);
        self.items.push_front(item);
        true
    }

    /// Enqueue an item at an arbitrary position, clamping `idx` to the
    /// current length so an out-of-range index appends instead of panicking.
    /// Returns `false` (rejecting it) when the queue is full, matching
    /// `push`'s cap semantics.
    pub fn insert(&mut self, idx: usize, mut item: QueueItem) -> bool {
        if self.items.len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.stamp(&mut item);
        let idx = idx.min(self.items.len());
        self.items.insert(idx, item);
        true
    }

    pub fn pop(&mut self) -> Option<QueueItem> {
        self.items.pop_front()
    }

    /// Look at the head of the queue without removing it.
    pub fn peek(&self) -> Option<&QueueItem> {
        self.items.front()
    }

    /// Pop the head only if it satisfies `pred`; otherwise leaves the queue
    /// untouched and returns `None`. Test-only today.
    #[cfg(test)]
    pub fn pop_if(&mut self, pred: impl Fn(&QueueItem) -> bool) -> Option<QueueItem> {
        if self.items.front().is_some_and(&pred) {
            self.items.pop_front()
        } else {
            None
        }
    }

    /// The first item (anywhere in the queue) that satisfies `pred`.
    pub fn find_first(&self, pred: impl Fn(&QueueItem) -> bool) -> Option<&QueueItem> {
        self.items.iter().find(|i| pred(i))
    }

    /// Remove and return the first item (anywhere in the queue) that
    /// satisfies `pred`.
    pub fn remove_first(&mut self, pred: impl Fn(&QueueItem) -> bool) -> Option<QueueItem> {
        let idx = self.items.iter().position(pred)?;
        self.items.remove(idx)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }


    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn snapshot(&self) -> Vec<QueueItem> {
        self.items.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::YouTube {
                url: "u".into(),
                video_id: "v".into(),
                title: title.into(),
                channel: "c".into(),
                thumbnail_url: None,
                duration_secs: 0,
            },
            "me".into(),
            1,
        )
    }

    #[test]
    fn duration_formats_as_clock_time() {
        let src = |secs| MediaSource::YouTube {
            url: "u".into(),
            video_id: "v".into(),
            title: "t".into(),
            channel: "c".into(),
            thumbnail_url: None,
            duration_secs: secs,
        };
        assert_eq!(src(59).display_duration().unwrap(), "0:59");
        assert_eq!(src(75).display_duration().unwrap(), "1:15");
        assert_eq!(src(3600).display_duration().unwrap(), "1:00:00");
        assert_eq!(src(3725).display_duration().unwrap(), "1:02:05");
        let file = MediaSource::File { filename: "f.mp3".into(), attachment_url: "a".into() };
        assert!(file.display_duration().is_none());
    }

    #[test]
    fn is_fifo() {
        let mut q = PriorityQueue::new();
        assert!(q.push(item("a")));
        assert!(q.push(item("b")));
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop().unwrap().source.display_title(), "a");
        assert_eq!(q.pop().unwrap().source.display_title(), "b");
        assert!(q.pop().is_none());
    }

    #[test]
    fn clear_empties() {
        let mut q = PriorityQueue::new();
        q.push(item("a"));
        q.clear();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn rejects_when_full() {
        let mut q = PriorityQueue::new();
        for i in 0..MAX_QUEUE_LEN {
            assert!(q.push(item(&i.to_string())), "should accept up to the cap");
        }
        assert!(!q.push(item("overflow")), "rejects past the cap");
        assert_eq!(q.len(), MAX_QUEUE_LEN);
    }

    #[test]
    fn push_front_takes_priority_over_push() {
        let mut q = PriorityQueue::new();
        assert!(q.push(item("a")));
        assert!(q.push_front(item("b")));
        let snap = q.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].source.display_title(), "b");
        assert_eq!(snap[1].source.display_title(), "a");
    }

    #[test]
    fn push_front_rejects_when_full() {
        let mut q = PriorityQueue::new();
        for i in 0..MAX_QUEUE_LEN {
            assert!(q.push(item(&i.to_string())), "should accept up to the cap");
        }
        assert!(!q.push_front(item("overflow")), "rejects past the cap");
        assert_eq!(q.len(), MAX_QUEUE_LEN);
    }

    fn spotify_item(uri: &str, title: &str) -> QueueItem {
        QueueItem::new(
            MediaSource::Spotify {
                uri: librespot_core::SpotifyUri::from_uri(uri).unwrap(),
                title: title.into(),
                artist: "artist".into(),
                album_art_url: None,
            },
            "me".into(),
            1,
        )
    }

    #[test]
    fn item_ids_are_unique_and_survive_reordering() {
        let mut q = PriorityQueue::new();
        assert!(q.push(item("a")));
        assert!(q.push(item("b")));
        assert!(q.push_front(item("c")));
        assert!(q.insert(1, item("d")));

        // Insertion order was a(1), b(2), c(3), d(4); queue order is now
        // [c, d, a, b] — each item keeps the id stamped at its insertion.
        let snap = q.snapshot();
        let pairs: Vec<(&str, u64)> =
            snap.iter().map(|i| (i.source.display_title(), i.item_id)).collect();
        assert_eq!(pairs, vec![("c", 3), ("d", 4), ("a", 1), ("b", 2)]);

        let mut ids: Vec<u64> = snap.iter().map(|i| i.item_id).collect();
        assert!(ids.iter().all(|&id| id != 0), "every insertion stamps a nonzero id");
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), snap.len(), "ids are unique");

        // Re-inserting a popped item stamps a fresh id: an id names one
        // residency in the queue, so it can never collide with a live one.
        let popped = q.pop().unwrap();
        assert_eq!(popped.item_id, 3);
        assert!(q.push(popped));
        assert_eq!(q.snapshot().last().unwrap().item_id, 5, "re-insertion restamps");
    }

    #[test]
    fn insert_at_one_keeps_head() {
        let mut q = PriorityQueue::new();
        assert!(q.push(item("a")));
        assert!(q.push(item("c")));
        assert!(q.insert(1, item("b")));
        let snap = q.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].source.display_title(), "a");
        assert_eq!(snap[1].source.display_title(), "b");
        assert_eq!(snap[2].source.display_title(), "c");
    }

    #[test]
    fn insert_rejects_when_full() {
        let mut q = PriorityQueue::new();
        for i in 0..MAX_QUEUE_LEN {
            assert!(q.push(item(&i.to_string())), "should accept up to the cap");
        }
        assert!(!q.insert(0, item("overflow")), "rejects past the cap");
        assert_eq!(q.len(), MAX_QUEUE_LEN);
    }

    #[test]
    fn peek_does_not_consume() {
        let mut q = PriorityQueue::new();
        assert!(q.peek().is_none());
        q.push(item("a"));
        q.push(item("b"));
        assert_eq!(q.peek().unwrap().source.display_title(), "a");
        assert_eq!(q.len(), 2, "peek doesn't drain");
        assert_eq!(q.peek().unwrap().source.display_title(), "a");
    }

    #[test]
    fn find_first_and_remove_first_reach_past_the_head() {
        let mut q = PriorityQueue::new();
        q.push(item("a"));
        q.push(spotify_item("spotify:track:11dFghVXANMlKmJXsNCbNl", "b"));
        q.push(item("c"));

        let is_spotify = |i: &QueueItem| matches!(i.source, MediaSource::Spotify { .. });
        assert_eq!(q.find_first(is_spotify).unwrap().source.display_title(), "b");
        assert_eq!(q.len(), 3, "find_first does not consume");

        let removed = q.remove_first(is_spotify).unwrap();
        assert_eq!(removed.source.display_title(), "b");
        let titles: Vec<_> = q.snapshot().iter().map(|i| i.source.display_title().to_string()).collect();
        assert_eq!(titles, vec!["a", "c"], "the surrounding order is kept");
        assert!(q.remove_first(is_spotify).is_none());
    }

    #[test]
    fn pop_if_only_pops_matching_head() {
        let mut q = PriorityQueue::new();
        q.push(item("a"));
        q.push(spotify_item("spotify:track:11dFghVXANMlKmJXsNCbNl", "b"));

        assert!(
            q.pop_if(|i| matches!(i.source, MediaSource::Spotify { .. })).is_none(),
            "head doesn't match, so nothing pops"
        );
        assert_eq!(q.len(), 2);

        let popped = q.pop_if(|i| matches!(i.source, MediaSource::YouTube { .. }));
        assert_eq!(popped.unwrap().source.display_title(), "a");
        assert_eq!(q.len(), 1);
        assert_eq!(q.peek().unwrap().source.display_title(), "b");
    }

    #[test]
    fn snapshot_preserves_order_without_draining() {
        let mut q = PriorityQueue::new();
        q.push(item("a"));
        q.push(item("b"));
        let snap = q.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].source.display_title(), "a");
        assert_eq!(q.len(), 2, "snapshot doesn't drain");
    }
}
