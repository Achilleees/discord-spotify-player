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

/// Bot-managed priority queue for YouTube/file tracks.
pub struct PriorityQueue {
    items: VecDeque<QueueItem>,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self { items: VecDeque::new() }
    }

    pub fn push(&mut self, item: QueueItem) {
        self.items.push_back(item);
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
