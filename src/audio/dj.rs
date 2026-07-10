use rand::RngExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing;

const DJ_CLIPS_DIR: &str = "/opt/openclaw/services/spotibot/dj-clips";
const DJ_CACHE_DIR: &str = "/opt/openclaw/services/spotibot/dj-cache";
const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: u32 = 2;

const TRACK_TEMPLATES: &[&str] = &[
    // Full title + artist
    "Next up, {title} by {artist}.",
    "Here we go, {title}, {artist}.",
    "Alright, {artist} with {title}.",
    "And now, {title} by {artist}.",
    "Coming in hot, {artist}, {title}.",
    "Let's keep it going with {title}, {artist}.",
    "{artist}. {title}. Here we go.",
    "This is {title} by {artist}.",
    "We got {artist} coming through with {title}.",
    "Up next, {artist}, {title}.",
    "{title}, {artist}. Enjoy.",
    "Rolling into {title} by {artist}.",
    "Now playing, {artist} with {title}.",
    "Switching it up. {title}, {artist}.",
    "{artist} on deck with {title}.",

    // Excitement / hype
    "Oh this is a good one. {title} by {artist}.",
    "This one's fire. {artist}, {title}.",
    "Yes! {title} by {artist}. Love this track.",
    "Oh we're going there. {artist}, {title}.",
    "Classic. {title}, {artist}.",
    "Big tune alert. {artist} with {title}.",
    "Turn it up. {title} by {artist}.",
    "This one hits different. {artist}, {title}.",
    "Absolute banger incoming. {title}, {artist}.",

    // Chill / laid back
    "Nice. {title} by {artist}.",
    "Smooth. {artist} with {title}.",
    "Good vibes. {title}, {artist}.",
    "Solid choice. {title} by {artist}.",
    "Can't go wrong with {artist}. {title}.",
    "Always a vibe. {title}, {artist}.",

    // Title only
    "{title}. You know it.",
    "Oh, {title}. Here we go.",
    "{title}. Say no more.",
    "{title}. Enough said.",

    // Artist only
    "{artist}. Need I say more?",
    "{artist} in the building.",
    "It's {artist} time.",
    "You already know. {artist}.",
    "{artist}. Let's ride.",

    // Short and punchy
    "{title}. {artist}. Go.",
    "{artist}. {title}. Vibes.",
    "Boom. {title}.",
    "Here it comes.",
    "And we keep it moving.",
    "Let's go.",
    "Alright alright alright.",
    "One more for the people.",
];

const QUEUED_TEMPLATES: &[&str] = &[
    "Next up, {title} by {artist}. Queued by {queued_by}.",
    "{queued_by} wants to hear {title} by {artist}. Let's go.",
    "Shoutout to {queued_by}. {title}, {artist}.",
    "Good pick {queued_by}. {artist} with {title}.",
    "{title} by {artist}, requested by {queued_by}.",
    "This one's for {queued_by}. {title}, {artist}.",
    "{queued_by} coming in with {title} by {artist}. Respect.",
    "Big request from {queued_by}. {artist} with {title}.",
    "{queued_by} said play {title}. Who am I to argue?",
    "From {queued_by}'s collection, {title} by {artist}.",
    "Thank {queued_by} for this one. {artist}, {title}.",
    "{queued_by} knows what's up. {title}.",
];

pub struct DJAnnouncer {
    greetings: Vec<Vec<f32>>,
    transitions: Vec<Vec<f32>>,
}

impl DJAnnouncer {
    pub fn new() -> Self {
        let _ = std::fs::create_dir_all(DJ_CACHE_DIR);
        let greetings = load_clips_from_dir(&format!("{}/greetings", DJ_CLIPS_DIR));
        let transitions = load_clips_from_dir(&format!("{}/transitions", DJ_CLIPS_DIR));

        tracing::info!(
            greetings = greetings.len(),
            transitions = transitions.len(),
            "DJ announcer loaded"
        );

        Self { greetings, transitions }
    }

    pub fn is_enabled(&self) -> bool {
        !self.greetings.is_empty() || !self.transitions.is_empty()
    }

    pub fn join_clip(&self) -> Option<Vec<f32>> {
        if self.greetings.is_empty() { return None; }
        let idx = rand::rng().random_range(0..self.greetings.len());
        Some(self.greetings[idx].clone())
    }

    pub fn transition_clip(&self) -> Option<Vec<f32>> {
        if self.transitions.is_empty() { return None; }
        let idx = rand::rng().random_range(0..self.transitions.len());
        Some(self.transitions[idx].clone())
    }

    /// Generate a track-specific announcement using Kokoro TTS.
    /// Picks a random template, fills in track info, generates audio.
    /// Returns None if generation fails (caller should fall back to transition_clip).
    pub async fn track_announce_clip(
        &self,
        title: &str,
        artist: &str,
        queued_by: &str,
    ) -> Option<Vec<f32>> {
        let text = if queued_by.is_empty() {
            let idx = rand::rng().random_range(0..TRACK_TEMPLATES.len());
            TRACK_TEMPLATES[idx]
                .replace("{title}", title)
                .replace("{artist}", artist)
        } else {
            let idx = rand::rng().random_range(0..QUEUED_TEMPLATES.len());
            QUEUED_TEMPLATES[idx]
                .replace("{title}", title)
                .replace("{artist}", artist)
                .replace("{queued_by}", queued_by)
        };

        tracing::info!(text = %text, "generating DJ track announcement");

        // Check cache first
        let hash = simple_hash(&text);
        let mp3_path = format!("{}/dj-{:016x}.mp3", DJ_CACHE_DIR, hash);

        if !std::path::Path::new(&mp3_path).exists() {
            // Generate with Kokoro via Unix socket daemon (fast, model pre-loaded)
            match kokoro_socket_generate(&text, &mp3_path).await {
                Ok(()) => {
                    tracing::info!(text = %text, path = %mp3_path, "kokoro daemon generated clip");
                }
                Err(e) => {
                    tracing::warn!(text = %text, error = %e, "kokoro daemon failed");
                    return self.transition_clip();
                }
            }
        }

        // Decode MP3 to PCM (fast, ok to block briefly)
        match tokio::task::spawn_blocking({
            let path = PathBuf::from(mp3_path.clone());
            move || decode_mp3_to_f32_stereo(&path)
        }).await {
            Ok(Ok(samples)) => {
                tracing::info!(
                    text = %text,
                    samples = samples.len(),
                    duration_s = samples.len() as f64 / (SAMPLE_RATE as f64 * CHANNELS as f64),
                    "track announcement ready"
                );
                Some(samples)
            }
            Ok(Err(e)) => {
                tracing::warn!(text = %text, error = %e, "decode failed");
                self.transition_clip()
            }
            Err(e) => {
                tracing::warn!(error = %e, "spawn_blocking failed");
                self.transition_clip()
            }
        }
    }
}

fn load_clips_from_dir(dir: &str) -> Vec<Vec<f32>> {
    let path = PathBuf::from(dir);
    if !path.exists() {
        tracing::warn!(dir, "DJ clips directory not found");
        return vec![];
    }

    let mut clips = Vec::new();
    let mut entries: Vec<_> = match std::fs::read_dir(&path) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            tracing::warn!(dir, error = %e, "failed to read DJ clips dir");
            return vec![];
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_path = entry.path();
        if file_path.extension().and_then(|e| e.to_str()) != Some("mp3") { continue; }
        match decode_mp3_to_f32_stereo(&file_path) {
            Ok(samples) => {
                tracing::debug!(file = %file_path.display(), samples = samples.len(), "loaded clip");
                clips.push(samples);
            }
            Err(e) => tracing::warn!(file = %file_path.display(), error = %e, "failed to load clip"),
        }
    }

    clips
}

fn decode_mp3_to_f32_stereo(path: &Path) -> Result<Vec<f32>, String> {
    let output = Command::new("ffmpeg")
        .args(["-i", path.to_str().unwrap_or(""), "-f", "f32le", "-acodec", "pcm_f32le",
               "-ac", &CHANNELS.to_string(), "-ar", &SAMPLE_RATE.to_string(), "-v", "quiet", "-"])
        .output()
        .map_err(|e| format!("ffmpeg failed: {}", e))?;

    if !output.status.success() {
        return Err(format!("ffmpeg error: {}", String::from_utf8_lossy(&output.stderr)));
    }

    let bytes = &output.stdout;
    if bytes.len() < 4 { return Err("no audio data".to_string()); }

    Ok(bytes.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn simple_hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}


const KOKORO_SOCKET: &str = "/opt/openclaw/services/spotibot/kokoro.sock";

/// Kokoro TTS is reached over a Unix domain socket, which only exists on the
/// Linux deployment. On other platforms DJ announcements are unavailable.
#[cfg(not(unix))]
async fn kokoro_socket_generate(_text: &str, _output_path: &str) -> Result<(), String> {
    Err(format!(
        "DJ announcements require the Kokoro unix socket ({KOKORO_SOCKET}), unavailable on this platform"
    ))
}

#[cfg(unix)]
async fn kokoro_socket_generate(text: &str, output_path: &str) -> Result<(), String> {
    use tokio::net::UnixStream;
    use tokio::io::{AsyncWriteExt, AsyncReadExt};

    let mut stream = UnixStream::connect(KOKORO_SOCKET)
        .await
        .map_err(|e| format!("socket connect failed: {}", e))?;

    let req = serde_json::json!({
        "text": text,
        "output": output_path
    });
    let msg = format!("{}\n", req.to_string());

    stream.write_all(msg.as_bytes()).await
        .map_err(|e| format!("socket write failed: {}", e))?;
    stream.shutdown().await
        .map_err(|e| format!("socket shutdown failed: {}", e))?;

    let mut response = String::new();
    stream.read_to_string(&mut response).await
        .map_err(|e| format!("socket read failed: {}", e))?;

    let parsed: serde_json::Value = serde_json::from_str(response.trim())
        .map_err(|e| format!("bad response: {} (raw: {})", e, response))?;

    if parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        Err(parsed.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string())
    }
}
