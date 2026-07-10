pub mod metadata;
pub mod feeder;

const DEFAULT_TMP_DIR: &str = "/tmp/spotibot-youtube";
const DEFAULT_COOKIES: &str = "/var/lib/spotibot/youtube-cookies.txt";

/// Scratch dir for downloaded audio, overridable via env (default is the VPS layout).
pub fn tmp_dir() -> String {
    std::env::var("YOUTUBE_TMP_DIR").unwrap_or_else(|_| DEFAULT_TMP_DIR.to_string())
}

/// yt-dlp cookies file, overridable via env; used only if it exists on disk.
pub fn cookies_path() -> String {
    std::env::var("YOUTUBE_COOKIES").unwrap_or_else(|_| DEFAULT_COOKIES.to_string())
}
