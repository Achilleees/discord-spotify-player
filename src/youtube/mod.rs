pub mod metadata;
pub mod feeder;
mod probe;

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

pub fn check_ytdlp_available() -> bool {
    std::process::Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn check_ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Remove leftover download files (`yt-*`, `file-*`) from the scratch dir.
/// Runs at startup: a crash or kill mid-download leaves partials that
/// otherwise accumulate forever.
pub fn sweep_tmp_dir() {
    let tmp = tmp_dir();
    let Ok(entries) = std::fs::read_dir(std::path::Path::new(&tmp)) else {
        return;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with("yt-") || name.starts_with("file-"))
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(removed, dir = %tmp, "swept stale download files");
    }
}
