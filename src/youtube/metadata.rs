use serde::Deserialize;

/// Maximum queue-able track duration (seconds); the env var
/// YOUTUBE_MAX_DURATION_SECS overrides it at runtime (checked below).
pub const YOUTUBE_MAX_DURATION_SECS: u64 = 7200;

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
    #[error("Only YouTube and SoundCloud URLs are supported.")]
    UnsupportedUrl,
    #[error("Couldn't find a track at that URL.")]
    NotFound,
    #[error("This video is age-restricted — the bot needs YouTube login cookies to play it (admin: set YOUTUBE_COOKIES).")]
    AgeRestricted,
    #[error("This track is unavailable.")]
    Unavailable,
    #[error("Track too long (max {0} min). Use Spotify for long content.")]
    TooLong(u64),
    #[error("Live streams aren't supported.")]
    LiveStream,
    #[error("Unsupported file type. Accepted: mp3, flac, ogg, wav, m4a, aac, opus, wma")]
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
}

/// Validate a /play URL before it reaches yt-dlp: https-only and an
/// allowlisted host. yt-dlp's generic extractor fetches ANY url server-side
/// (SSRF: loopback services, cloud metadata endpoints) and reflects the
/// fetched page's metadata into the Now Playing embed — so unsupported hosts
/// must be rejected before any subprocess spawns. Returns the canonicalized
/// URL string to pass along.
pub fn validate_play_url(input: &str) -> Result<String, YoutubeError> {
    let trimmed = input.trim();
    // Accept a scheme-less paste ("youtube.com/watch?v=…").
    let parsed = url::Url::parse(trimmed)
        .or_else(|_| url::Url::parse(&format!("https://{trimmed}")))
        .map_err(|_| YoutubeError::UnsupportedUrl)?;
    if parsed.scheme() != "https" {
        return Err(YoutubeError::UnsupportedUrl);
    }
    let host = parsed
        .host_str()
        .ok_or(YoutubeError::UnsupportedUrl)?
        .to_ascii_lowercase();
    // Dot-anchored suffix match so "evilyoutube.com" can't pass.
    let is_or_sub = |root: &str| host == root || host.ends_with(&format!(".{root}"));
    if is_or_sub("youtube.com") || host == "youtu.be" || is_or_sub("soundcloud.com") {
        Ok(parsed.into())
    } else {
        Err(YoutubeError::UnsupportedUrl)
    }
}

/// Concurrent yt-dlp probe cap: each /play spawns a subprocess (network
/// fetch and JSON parse) before the queue cap applies, so without a bound a
/// rapid caller drives unbounded CPU/PID pressure on the shared VPS.
fn probe_permits() -> &'static tokio::sync::Semaphore {
    static PERMITS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    PERMITS.get_or_init(|| tokio::sync::Semaphore::new(3))
}

/// Run `yt-dlp --dump-json <url>` and parse the result (metadata only, no download).
pub async fn fetch_youtube_metadata(url: &str) -> Result<YoutubeMetadata, YoutubeError> {
    let url = validate_play_url(url)?;
    let url = url.as_str();
    let _permit = probe_permits()
        .acquire()
        .await
        .expect("probe semaphore is never closed");
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

    // No age_limit check here: getting metadata JSON for an age-gated video
    // already required authenticated (cookie) access — the anonymous case
    // fails earlier via stderr. Rejecting on age_limit would block exactly
    // the videos that configured cookies unlock.

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
