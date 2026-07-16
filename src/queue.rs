use std::collections::VecDeque;

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
}

impl MediaSource {
    pub fn display_title(&self) -> &str {
        match self {
            MediaSource::YouTube { title, .. } => title,
            MediaSource::File { filename, .. } => filename,
        }
    }

    pub fn display_subtitle(&self) -> String {
        match self {
            MediaSource::YouTube { channel, .. } => channel.clone(),
            MediaSource::File { .. } => "File upload".to_string(),
        }
    }

    /// Track length as "M:SS" (or "H:MM:SS"); None for file uploads, whose
    /// length isn't known until decode.
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
        }
    }

    pub fn embed_color(&self) -> u32 {
        match self {
            MediaSource::YouTube { .. } => 0xFF0000,
            MediaSource::File { .. } => 0x5865F2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct QueueItem {
    pub source: MediaSource,
    pub queued_by: String,
    pub queued_by_id: u64,
}

/// Maximum queued items (matches nob's unified-queue cap).
pub const MAX_QUEUE_LEN: usize = 500;

/// Bot-managed priority queue for YouTube/file tracks.
pub struct PriorityQueue {
    items: VecDeque<QueueItem>,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self { items: VecDeque::new() }
    }

    /// Enqueue an item. Returns `false` (rejecting it) when the queue is full,
    /// so an in-voice user can't grow it without bound.
    pub fn push(&mut self, item: QueueItem) -> bool {
        if self.items.len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.items.push_back(item);
        true
    }

    pub fn pop(&mut self) -> Option<QueueItem> {
        self.items.pop_front()
    }

    pub fn len(&self) -> usize {
        self.items.len()
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
        QueueItem {
            source: MediaSource::YouTube {
                url: "u".into(),
                video_id: "v".into(),
                title: title.into(),
                channel: "c".into(),
                thumbnail_url: None,
                duration_secs: 0,
            },
            queued_by: "me".into(),
            queued_by_id: 1,
        }
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
