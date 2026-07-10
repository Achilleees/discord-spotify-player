use serde::Deserialize;
use crate::queue::YOUTUBE_MAX_DURATION_SECS;

#[derive(Debug, Clone)]
pub struct YoutubeMetadata {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub thumbnail_url: Option<String>,
    pub duration_secs: u64,
    pub webpage_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum YoutubeError {
    #[error("Couldn't find a video at that URL.")]
    NotFound,
    #[error("This video is age-restricted and can't be played.")]
    AgeRestricted,
    #[error("This video is unavailable.")]
    Unavailable,
    #[error("Video too long (max {0} min). Use Spotify for long content.")]
    TooLong(u64),
    #[error("Live streams aren't supported.")]
    LiveStream,
    #[error("Download failed: {0}")]
    #[allow(dead_code)]
    DownloadFailed(String),
    #[error("Unsupported file type. Accepted: mp3, flac, ogg, wav, m4a, aac, opus")]
    InvalidFileType,
    #[error("File too large (max 50MB).")]
    FileTooLarge,
    #[error("Network error: {0}")]
    Network(String),
    #[error("Parse error: {0}")]
    Parse(String),
}

#[derive(Deserialize)]
struct YtDlpJson {
    id: String,
    title: String,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    is_live: Option<bool>,
    #[serde(default)]
    age_limit: Option<u32>,
}

/// Run `yt-dlp --dump-json <url>` and parse the result (metadata only, no download).
pub async fn fetch_youtube_metadata(url: &str) -> Result<YoutubeMetadata, YoutubeError> {
    let cookies_path = crate::youtube::cookies_path();
    let mut args = vec!["--dump-json", "--no-playlist", "--flat-playlist", "--remote-components", "ejs:github"];
    if std::path::Path::new(&cookies_path).exists() {
        args.extend(["--cookies", &cookies_path]);
    }
    // `--` terminates option parsing so a `-`-leading URL can't be a flag.
    args.push("--");
    args.push(url);
    let output = tokio::process::Command::new("yt-dlp")
        .args(&args)
        .output()
        .await
        .map_err(|e| YoutubeError::Network(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_lowercase();
        tracing::warn!(stderr = %stderr, "yt-dlp failed");
        if stderr_lower.contains("age") && (stderr_lower.contains("sign in") || stderr_lower.contains("confirm your age")) {
            return Err(YoutubeError::AgeRestricted);
        }
        if stderr_lower.contains("unavailable") || stderr_lower.contains("private") || stderr_lower.contains("removed") {
            return Err(YoutubeError::Unavailable);
        }
        // Log the raw stderr server-side; return a generic message so cookie
        // paths / extractor internals aren't leaked to the requester.
        return Err(YoutubeError::Network(
            "couldn't fetch that link — check the URL and try again".to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or("");
    if first_line.is_empty() {
        return Err(YoutubeError::NotFound);
    }

    let raw: YtDlpJson = serde_json::from_str(first_line)
        .map_err(|e| YoutubeError::Parse(e.to_string()))?;

    if raw.is_live == Some(true) || raw.duration.is_none() {
        return Err(YoutubeError::LiveStream);
    }

    let duration_secs = raw.duration.unwrap_or(0.0) as u64;
    let max = std::env::var("YOUTUBE_MAX_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(YOUTUBE_MAX_DURATION_SECS);

    if duration_secs > max {
        return Err(YoutubeError::TooLong(max / 60));
    }

    if raw.age_limit.unwrap_or(0) >= 18 {
        return Err(YoutubeError::AgeRestricted);
    }

    let channel = raw.channel
        .or(raw.uploader)
        .unwrap_or_else(|| "Unknown channel".to_string());

    let webpage_url = raw.webpage_url
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", raw.id));

    Ok(YoutubeMetadata {
        video_id: raw.id,
        title: raw.title,
        channel,
        thumbnail_url: raw.thumbnail,
        duration_secs,
        webpage_url,
    })
}

/// Validate a file attachment for playback.
pub fn validate_attachment(filename: &str, size_bytes: u64) -> Result<String, YoutubeError> {
    const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
    const ALLOWED_EXTS: &[&str] = &["mp3", "flac", "ogg", "opus", "wav", "m4a", "aac", "wma"];

    if size_bytes > MAX_FILE_BYTES {
        return Err(YoutubeError::FileTooLarge);
    }

    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    if !ALLOWED_EXTS.contains(&ext.as_str()) {
        return Err(YoutubeError::InvalidFileType);
    }

    Ok(ext)
}
