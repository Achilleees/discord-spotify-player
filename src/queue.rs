use std::collections::VecDeque;

/// Maximum YouTube duration (seconds). Configurable via env YOUTUBE_MAX_DURATION_SECS.
pub const YOUTUBE_MAX_DURATION_SECS: u64 = 7200; // 2 hours default

#[derive(Clone, Debug)]
pub enum MediaSource {
    YouTube {
        url: String,
        video_id: String,
        title: String,
        channel: String,
        thumbnail_url: Option<String>,
        #[allow(dead_code)]
        duration_secs: u64,
    },
    File {
        filename: String,
        attachment_url: String,
        #[allow(dead_code)]
        content_type: Option<String>,
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
    #[allow(dead_code)]
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
