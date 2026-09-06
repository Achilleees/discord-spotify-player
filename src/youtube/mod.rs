pub mod metadata;
pub mod feeder;
mod probe;

/// This process's resolved scratch directory; locked before startup cleanup.
pub fn tmp_dir() -> String {
    crate::runtime::paths().youtube_tmp.to_string_lossy().into_owned()
}

/// This process's explicit cookies path, used only if the file exists.
pub fn cookies_path() -> String {
    crate::runtime::paths().youtube_cookies.to_string_lossy().into_owned()
}

/// Extractor/signature cache stays within the locked per-instance scratch dir.
pub(super) fn extractor_cache_path() -> std::path::PathBuf {
    crate::runtime::paths().youtube_tmp.join("extractor-cache")
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
