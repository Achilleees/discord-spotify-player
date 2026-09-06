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
    #[error("Music lookup is busy — try again in a moment.")]
    Busy,
    #[error("Music lookup took too long — try again.")]
    Timeout,
    #[error("That lookup returned too much data — try a different track.")]
    ResponseTooLarge,
    #[error("Enter a song or artist name (up to 200 characters).")]
    InvalidQuery,
    #[error("Only Spotify track, YouTube and SoundCloud links are supported.")]
    UnsupportedUrl,
    #[error("Couldn't find a track at that URL.")]
    NotFound,
    #[error("This video is age-restricted — the bot needs YouTube login cookies to play it (admin: set YOUTUBE_COOKIES).")]
    AgeRestricted,
    #[error("This track is unavailable.")]
    Unavailable,
    #[error("This track is DRM-protected and can't be played.")]
    DrmProtected,
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
    #[serde(default)]
    live_status: Option<String>,
}

pub const SEARCH_RESULTS: usize = 5;

/// A discovery result, not authorization to play. Selection resolves the
/// canonical URL again to enforce the full media policy at enqueue time.
#[derive(Debug, Clone)]
pub struct YoutubeSearchResult {
    pub url: String,
    pub title: String,
    pub channel: String,
    pub duration_secs: Option<u64>,
}

fn search_input(query: &str) -> Result<String, YoutubeError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 200 || query.chars().any(char::is_control) {
        return Err(YoutubeError::InvalidQuery);
    }
    // Always construct the extractor prefix locally. User text can never
    // select another extractor, change the result limit or become CLI flags.
    Ok(format!("ytsearch{SEARCH_RESULTS}:{query}"))
}

fn search_results_from_json(output: &str, max_secs: u64) -> Vec<YoutubeSearchResult> {
    let mut results = Vec::new();
    for line in output.lines().take(20) {
        let Ok(raw) = serde_json::from_str::<YtDlpJson>(line) else { continue };
        if raw.id.len() != 11
            || !raw.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || raw.title.trim().is_empty()
            || raw.is_live == Some(true)
            || matches!(raw.live_status.as_deref(), Some("is_live" | "is_upcoming"))
            || raw.duration.is_some_and(|d| !d.is_finite() || d < 0.0 || d > max_secs as f64)
        {
            continue;
        }
        // Do not trust a result's arbitrary `url`/`webpage_url` as a playback
        // destination. ytsearch names YouTube videos; construct that URL.
        let url = format!("https://www.youtube.com/watch?v={}", raw.id);
        if results.iter().any(|r: &YoutubeSearchResult| r.url == url) {
            continue;
        }
        results.push(YoutubeSearchResult {
            url,
            title: raw.title.chars().take(240).collect(),
            channel: raw.channel.or(raw.uploader).unwrap_or_else(|| "Unknown artist".into())
                .chars().take(80).collect(),
            duration_secs: raw.duration.map(|d| d as u64),
        });
        if results.len() == SEARCH_RESULTS { break; }
    }
    results
}

pub async fn search_youtube(query: &str) -> Result<Vec<YoutubeSearchResult>, YoutubeError> {
    let input = search_input(query)?;
    let output = super::probe::run(&input).await?;
    let max = resolve_max_duration_secs(crate::runtime::paths().youtube_max_duration.as_deref());
    Ok(search_results_from_json(&output, max))
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

/// Classify a failed yt-dlp probe from its stderr. The returned error is
/// user-facing, so the
/// fallback is deliberately generic — cookie paths and extractor internals
/// must not reach the requester.
pub(super) fn classify_ytdlp_stderr(stderr: &str) -> YoutubeError {
    let s = stderr.to_lowercase();
    if s.contains("age") && (s.contains("sign in") || s.contains("confirm your age")) {
        return YoutubeError::AgeRestricted;
    }
    if s.contains("drm protected") {
        return YoutubeError::DrmProtected;
    }
    if s.contains("unavailable") || s.contains("private") || s.contains("removed") {
        return YoutubeError::Unavailable;
    }
    YoutubeError::Network("couldn't fetch that link — check the URL and try again".to_string())
}

/// The YOUTUBE_MAX_DURATION_SECS override; unset or unparseable falls back to
/// the default cap.
fn resolve_max_duration_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(YOUTUBE_MAX_DURATION_SECS)
}

/// Map one line of `yt-dlp --dump-json` output to track metadata, enforcing
/// the live-stream and duration policies.
fn metadata_from_json(first_line: &str, max_secs: u64) -> Result<YoutubeMetadata, YoutubeError> {
    if first_line.is_empty() {
        return Err(YoutubeError::NotFound);
    }

    let raw: YtDlpJson =
        serde_json::from_str(first_line).map_err(|e| YoutubeError::Parse(e.to_string()))?;

    if raw.is_live == Some(true)
        || matches!(raw.live_status.as_deref(), Some("is_live" | "is_upcoming"))
        || raw.duration.is_none()
    {
        return Err(YoutubeError::LiveStream);
    }

    let duration_secs = raw.duration.unwrap_or(0.0) as u64;
    if duration_secs > max_secs {
        return Err(YoutubeError::TooLong(max_secs / 60));
    }

    // The probe JSON's age_limit field is deliberately not parsed or checked:
    // metadata success already means the video is playable under the current
    // cookie config (the anonymous age-gated case fails earlier, via stderr).
    // Pinned by tests::age_limit_field_is_ignored — a defensive age_limit
    // reject here would block exactly the videos configured cookies unlock.

    let channel = raw
        .channel
        .or(raw.uploader)
        .unwrap_or_else(|| "Unknown channel".to_string());

    let webpage_url = raw
        .webpage_url
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

/// Run `yt-dlp --dump-json <url>` and parse the result (metadata only, no download).
pub async fn fetch_youtube_metadata(url: &str) -> Result<YoutubeMetadata, YoutubeError> {
    let url = validate_play_url(url)?;
    let stdout = super::probe::run(&url).await?;
    let first_line = stdout.lines().next().unwrap_or("");
    let max =
        resolve_max_duration_secs(crate::runtime::paths().youtube_max_duration.as_deref());
    metadata_from_json(first_line, max)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn search_row(id: &str, duration: Option<f64>) -> serde_json::Value {
        serde_json::json!({"id": id, "title": "A track", "uploader": "An artist", "duration": duration})
    }

    #[test]
    fn search_cannot_change_the_extractor_or_result_limit() {
        assert_eq!(search_input("  --exec echo  ").unwrap(), "ytsearch5:--exec echo");
        assert_eq!(search_input("ytsearch999:track").unwrap(), "ytsearch5:ytsearch999:track");
        assert!(search_input(" \t ").is_err());
        assert!(search_input("one\ntwo").is_err());
        assert!(search_input(&"x".repeat(201)).is_err());
    }

    #[test]
    fn search_skips_bad_live_and_long_results_without_losing_valid_choices() {
        let mut live = search_row("bbbbbbbbbbb", Some(10.0));
        live["live_status"] = "is_live".into();
        let mut destination = search_row("ddddddddddd", None);
        destination["webpage_url"] = "https://127.0.0.1/private".into();
        let lines = [
            "not JSON".into(), search_row("bad/id", Some(10.0)).to_string(),
            live.to_string(), search_row("ccccccccccc", Some(999.0)).to_string(),
            destination.to_string(), search_row("aaaaaaaaaaa", Some(123.0)).to_string(),
            search_row("aaaaaaaaaaa", Some(123.0)).to_string(),
        ].join("\n");
        let results = search_results_from_json(&lines, 180);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://www.youtube.com/watch?v=ddddddddddd");
        assert_eq!(results[0].duration_secs, None);
        assert_eq!(results[1].channel, "An artist");
        assert_eq!(results[1].duration_secs, Some(123));
    }

    #[test]
    fn search_caps_the_number_and_size_of_choices() {
        let lines = (0..10).map(|i| {
            let mut row = search_row(&format!("test{i:07}"), Some(20.0));
            row["title"] = "🎵".repeat(1000).into();
            row["channel"] = "x".repeat(500).into();
            row.to_string()
        }).collect::<Vec<_>>().join("\n");
        let results = search_results_from_json(&lines, 180);
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r| r.title.chars().count() <= 240 && r.channel.len() <= 80));
    }

    // --- validate_play_url: the SSRF gate in front of yt-dlp ---

    #[test]
    fn allows_youtube_soundcloud_and_subdomains() {
        for url in [
            "https://www.youtube.com/watch?v=abc",
            "https://youtube.com/watch?v=abc",
            "https://music.youtube.com/watch?v=abc",
            "https://youtu.be/abc",
            "https://soundcloud.com/artist/track",
            "https://on.soundcloud.com/xyz",
        ] {
            assert!(validate_play_url(url).is_ok(), "should allow {url}");
        }
    }

    #[test]
    fn accepts_schemeless_paste_as_https() {
        let out = validate_play_url("youtube.com/watch?v=abc").unwrap();
        assert!(out.starts_with("https://youtube.com/"), "got {out}");
    }

    #[test]
    fn rejects_lookalike_hosts() {
        // Dot-anchored suffix match: "evilyoutube.com" must not pass.
        assert!(validate_play_url("https://evilyoutube.com/watch?v=abc").is_err());
        assert!(validate_play_url("https://youtube.com.evil.example/watch").is_err());
        assert!(validate_play_url("https://notyoutu.be/abc").is_err());
    }

    #[test]
    fn rejects_non_https_and_internal_targets() {
        // The SSRF shapes the gate exists for: plain-http, loopback services,
        // and cloud metadata endpoints.
        assert!(validate_play_url("http://youtube.com/watch?v=abc").is_err());
        assert!(validate_play_url("https://127.0.0.1:18789/panel").is_err());
        assert!(validate_play_url("https://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_play_url("file:///etc/passwd").is_err());
        assert!(validate_play_url("").is_err());
    }

    // --- classify_ytdlp_stderr: user-facing error routing ---

    #[test]
    fn classifies_age_gate_only_with_both_markers() {
        assert!(matches!(
            classify_ytdlp_stderr("ERROR: Sign in to confirm your age. This video may be inappropriate"),
            YoutubeError::AgeRestricted
        ));
        // "age" alone (e.g. a channel named "Golden Age") must not classify.
        assert!(matches!(
            classify_ytdlp_stderr("ERROR: something about age"),
            YoutubeError::Network(_)
        ));
    }

    #[test]
    fn classifies_drm_protected() {
        assert!(matches!(
            classify_ytdlp_stderr("ERROR: [soundcloud] 2119293606: This video is DRM protected"),
            YoutubeError::DrmProtected
        ));
    }

    #[test]
    fn classifies_unavailable_variants() {
        for s in ["Video unavailable", "This video is private", "removed by the uploader"] {
            assert!(
                matches!(classify_ytdlp_stderr(s), YoutubeError::Unavailable),
                "misrouted: {s}"
            );
        }
    }

    #[test]
    fn unknown_stderr_stays_generic() {
        // The generic fallback is the no-leak rule: whatever yt-dlp printed
        // (cookie paths, extractor internals) must not reach the requester.
        let err = classify_ytdlp_stderr("ERROR: /var/lib/spotibot/youtube-cookies.txt is malformed");
        match err {
            YoutubeError::Network(msg) => assert!(!msg.contains("cookies"), "leaked: {msg}"),
            other => panic!("expected Network, got {other:?}"),
        }
    }

    // --- metadata_from_json: policy enforcement on the probe result ---

    fn json(fields: &str) -> String {
        format!(r#"{{"id":"vid1","title":"Title"{fields}}}"#)
    }

    #[test]
    fn maps_a_full_result() {
        let m = metadata_from_json(
            &json(r##","channel":"Chan","uploader":"Up","duration":213.4,"thumbnail":"https://i/t.jpg","webpage_url":"https://w""##),
            7200,
        )
        .unwrap();
        assert_eq!(m.video_id, "vid1");
        assert_eq!(m.channel, "Chan", "channel wins over uploader");
        assert_eq!(m.duration_secs, 213);
        assert_eq!(m.webpage_url, "https://w");
    }

    #[test]
    fn falls_back_to_uploader_then_unknown() {
        let m = metadata_from_json(&json(r#","uploader":"Up","duration":10.0"#), 7200).unwrap();
        assert_eq!(m.channel, "Up");
        let m = metadata_from_json(&json(r#","duration":10.0"#), 7200).unwrap();
        assert_eq!(m.channel, "Unknown channel");
        assert_eq!(m.webpage_url, "https://www.youtube.com/watch?v=vid1");
    }

    #[test]
    fn rejects_live_streams_and_missing_duration() {
        assert!(matches!(
            metadata_from_json(&json(r#","is_live":true,"duration":10.0"#), 7200),
            Err(YoutubeError::LiveStream)
        ));
        // No duration at all is treated as live/undecodable, not 0 seconds.
        assert!(matches!(
            metadata_from_json(&json(""), 7200),
            Err(YoutubeError::LiveStream)
        ));
    }

    #[test]
    fn enforces_the_duration_cap() {
        assert!(metadata_from_json(&json(r#","duration":7200.0"#), 7200).is_ok());
        match metadata_from_json(&json(r#","duration":7201.0"#), 7200) {
            Err(YoutubeError::TooLong(mins)) => assert_eq!(mins, 120),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_malformed_probe_output() {
        assert!(matches!(metadata_from_json("", 7200), Err(YoutubeError::NotFound)));
        assert!(matches!(
            metadata_from_json("not json", 7200),
            Err(YoutubeError::Parse(_))
        ));
    }

    #[test]
    fn age_limit_field_is_ignored() {
        // Reaching metadata already proves the video is playable with the
        // current cookie config; a "defensive" age_limit reject here would
        // block exactly the videos that configured cookies unlock.
        let m = metadata_from_json(&json(r#","duration":10.0,"age_limit":18"#), 7200);
        assert!(m.is_ok(), "age-gated metadata that probed successfully must play");
    }

    #[test]
    fn duration_cap_env_override_parses_or_defaults() {
        assert_eq!(resolve_max_duration_secs(None), YOUTUBE_MAX_DURATION_SECS);
        assert_eq!(resolve_max_duration_secs(Some("3600")), 3600);
        assert_eq!(
            resolve_max_duration_secs(Some("two hours")),
            YOUTUBE_MAX_DURATION_SECS
        );
    }

    // --- validate_attachment ---

    #[test]
    fn enforces_the_size_cap_boundary() {
        const MAX: u64 = 50 * 1024 * 1024;
        assert!(validate_attachment("a.mp3", MAX).is_ok(), "exactly 50MB is allowed");
        assert!(matches!(
            validate_attachment("a.mp3", MAX + 1),
            Err(YoutubeError::FileTooLarge)
        ));
    }

    #[test]
    fn allowlists_extensions_case_insensitively() {
        for f in ["a.mp3", "a.flac", "a.ogg", "a.opus", "a.wav", "a.m4a", "a.aac", "a.wma"] {
            assert!(validate_attachment(f, 1).is_ok(), "should allow {f}");
        }
        assert_eq!(validate_attachment("A.MP3", 1).unwrap(), "mp3", "lowercased ext");
        for f in ["a.exe", "a.mp4", "a.txt", "a"] {
            assert!(
                matches!(validate_attachment(f, 1), Err(YoutubeError::InvalidFileType)),
                "should reject {f}"
            );
        }
    }

    #[test]
    fn dotless_filename_matching_an_extension_is_accepted() {
        // rsplit('.').next() on a dotless name yields the whole name, so a file
        // literally named "mp3" passes the allowlist. Pinned as accepted: the
        // extension only picks the decode hint — ffmpeg sniffs the real format,
        // and a wrong hint fails the decode, not the gate.
        assert_eq!(validate_attachment("mp3", 1).unwrap(), "mp3");
    }
}
