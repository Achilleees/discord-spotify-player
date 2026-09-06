use crate::audio_bridge::AudioBridge;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// Local typed alias of the canonical bridge format.
const CHANNELS: u64 = crate::audio_bridge::CHANNELS as u64;
const READ_CHUNK_BYTES: usize = 8192;

#[derive(Debug)]
pub enum FeederError {
    Cancelled,
    DownloadFailed(String),
    ConvertFailed(String),
    Io(std::io::Error),
}

impl std::fmt::Display for FeederError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeederError::Cancelled => write!(f, "cancelled"),
            FeederError::DownloadFailed(s) => write!(f, "download failed: {}", s),
            FeederError::ConvertFailed(s) => write!(f, "convert failed: {}", s),
            FeederError::Io(e) => write!(f, "io: {}", e),
        }
    }
}

fn ensure_tmp_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(crate::youtube::tmp_dir())
}

/// Prepend `carry` to `chunk`, decode whole stereo frames (two LE f32s = 8
/// bytes), and leave the 0-7 byte tail in `carry` for the next read. Framing —
/// not just f32 alignment — is the invariant: emitting an odd number of whole
/// samples would make the bridge drop one to stay even and permanently swap
/// L/R from that point on.
fn split_stereo_frames(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<f32> {
    let mut bytes = Vec::with_capacity(carry.len() + chunk.len());
    bytes.extend_from_slice(carry);
    bytes.extend_from_slice(chunk);
    let usable = bytes.len() - (bytes.len() % 8);
    let samples: Vec<f32> = bytes[..usable]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect();
    carry.clear();
    carry.extend_from_slice(&bytes[usable..]);
    samples
}

/// Best-effort removal of tmp files matching `prefix` — cancelled or failed
/// downloads must not accumulate partials (yt-dlp runs with --no-part, so a
/// killed download leaves its output behind).
fn remove_partials(prefix: &str) {
    let tmp = crate::youtube::tmp_dir();
    if let Ok(entries) = std::fs::read_dir(std::path::Path::new(&tmp)) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

async fn download_youtube(url: &str, token: &CancellationToken) -> Result<std::path::PathBuf, FeederError> {
    // Defense in depth: the queued URL normally comes from yt-dlp's own
    // metadata output, but never hand the generic extractor an arbitrary URL.
    let url = crate::youtube::metadata::validate_play_url(url)
        .map_err(|_| FeederError::DownloadFailed("unsupported URL".to_string()))?;
    ensure_tmp_dir().map_err(FeederError::Io)?;
    let id = Uuid::new_v4().to_string();
    let output_template = format!("{}/yt-{}.%(ext)s", crate::youtube::tmp_dir(), id);

    let cookies_path = crate::youtube::cookies_path();
    let mut args = vec!["--ignore-config".to_string(), "--cache-dir".to_string(), super::extractor_cache_path().to_string_lossy().into_owned(), "-f".to_string(), "bestaudio*".to_string(), "--no-playlist".to_string(), "--no-part".to_string(), "--remote-components".to_string(), "ejs:github".to_string(), "-o".to_string(), output_template.clone()];
    if std::path::Path::new(&cookies_path).exists() {
        args.extend(["--cookies".to_string(), cookies_path.clone()]);
    }
    // `--` terminates option parsing so a URL starting with `-` can't be read
    // as a yt-dlp flag.
    args.push("--".to_string());
    args.push(url.to_string());
    let mut child = Command::new("yt-dlp")
        .args(&args)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| FeederError::DownloadFailed(e.to_string()))?;

    let prefix = format!("yt-{}", id);
    tokio::select! {
        status = child.wait() => {
            let s = status.map_err(FeederError::Io)?;
            if !s.success() {
                remove_partials(&prefix);
                return Err(FeederError::DownloadFailed(format!("yt-dlp exit code: {:?}", s.code())));
            }
        }
        _ = token.cancelled() => {
            let _ = child.kill().await;
            remove_partials(&prefix);
            return Err(FeederError::Cancelled);
        }
    }

    find_downloaded_file(&prefix)
}

async fn download_attachment(url: &str, ext: &str, token: &CancellationToken) -> Result<std::path::PathBuf, FeederError> {
    ensure_tmp_dir().map_err(FeederError::Io)?;
    let id = Uuid::new_v4().to_string();
    let path = std::path::PathBuf::from(format!("{}/file-{}.{}", crate::youtube::tmp_dir(), id, ext));

    let client = reqwest::Client::new();
    let download_fut = async {
        let resp = client.get(url).send().await.map_err(|e| FeederError::DownloadFailed(e.to_string()))?;
        // Reject 4xx/5xx up front so an error body isn't written and decoded as
        // (empty) audio, which would look like a successfully played track.
        let resp = resp.error_for_status().map_err(|e| FeederError::DownloadFailed(e.to_string()))?;
        let bytes = resp.bytes().await.map_err(|e| FeederError::DownloadFailed(e.to_string()))?;
        tokio::fs::write(&path, &bytes).await.map_err(FeederError::Io)?;
        Ok::<_, FeederError>(())
    };

    tokio::select! {
        result = download_fut => {
            if let Err(e) = result {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        }
        _ = token.cancelled() => {
            let _ = std::fs::remove_file(&path);
            return Err(FeederError::Cancelled);
        }
    }

    Ok(path)
}

fn find_downloaded_file(prefix: &str) -> Result<std::path::PathBuf, FeederError> {
    let tmp = crate::youtube::tmp_dir();
    let dir = std::path::Path::new(&tmp);
    for entry in std::fs::read_dir(dir).map_err(FeederError::Io)? {
        let entry = entry.map_err(FeederError::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(prefix) {
            return Ok(entry.path());
        }
    }
    Err(FeederError::DownloadFailed("downloaded file not found".to_string()))
}

async fn feed_pcm_to_bridge(
    input_path: &std::path::Path,
    bridge: &Arc<AudioBridge>,
    token: &CancellationToken,
    paused: &Arc<AtomicBool>,
) -> Result<(), FeederError> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-i", input_path.to_str().unwrap_or(""),
            "-f", "f32le",
            "-ar", "44100",
            "-ac", "2",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| FeederError::ConvertFailed(e.to_string()))?;

    let mut stdout = child.stdout.take()
        .ok_or_else(|| FeederError::ConvertFailed("no stdout".to_string()))?;

    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    // Trailing bytes of a read that missed a frame boundary — see
    // split_stereo_frames for the L/R invariant.
    let mut carry: Vec<u8> = Vec::with_capacity(8);
    let mut frames_sent: u64 = 0;
    let mut start = Instant::now();

    loop {
        // Handle pause. Rebase `start` by the paused duration so the real-time
        // target (start + frames_sent/rate) doesn't fall into the past and make
        // the reader rush at full speed on resume, overflowing the bridge.
        if paused.load(Ordering::Relaxed) {
            let pause_began = Instant::now();
            while paused.load(Ordering::Relaxed) {
                if token.is_cancelled() {
                    let _ = child.kill().await;
                    return Err(FeederError::Cancelled);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            start += pause_began.elapsed();
        }

        if token.is_cancelled() {
            let _ = child.kill().await;
            return Err(FeederError::Cancelled);
        }

        let n = tokio::select! {
            result = stdout.read(&mut buf) => {
                result.map_err(FeederError::Io)?
            }
            _ = token.cancelled() => {
                let _ = child.kill().await;
                return Err(FeederError::Cancelled);
            }
        };

        if n == 0 {
            break;
        }

        let samples = split_stereo_frames(&mut carry, &buf[..n]);
        bridge.push_samples(&samples);

        // Real-time pacing (mirrors DiscordSink)
        let samples_in_chunk = samples.len() as u64;
        let frames_in_chunk = samples_in_chunk / CHANNELS;
        frames_sent = frames_sent.saturating_add(frames_in_chunk);

        let target = crate::audio_bridge::playout_deadline(start, frames_sent);
        let now = Instant::now();
        if target > now {
            let remaining = target - now;
            if remaining > Duration::from_millis(2) {
                tokio::time::sleep(remaining - Duration::from_millis(1)).await;
                while Instant::now() < target {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    // EOF alone doesn't mean success: corrupt or non-audio input makes ffmpeg
    // exit non-zero with little or no output, which must reach the user as a
    // failure rather than a silently "finished" track.
    let status = child.wait().await.map_err(FeederError::Io)?;
    if !status.success() {
        return Err(FeederError::ConvertFailed(format!(
            "ffmpeg exit code: {:?}",
            status.code()
        )));
    }
    Ok(())
}

/// Download a YouTube URL and feed into bridge. Blocks until complete or cancelled.
pub async fn feed_youtube_to_bridge(
    url: &str,
    bridge: Arc<AudioBridge>,
    token: CancellationToken,
    paused: Arc<AtomicBool>,
) -> Result<(), FeederError> {
    let path = download_youtube(url, &token).await?;
    let result = feed_pcm_to_bridge(&path, &bridge, &token, &paused).await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

/// Download a file attachment and feed into bridge. Blocks until complete or cancelled.
pub async fn feed_file_to_bridge(
    attachment_url: &str,
    ext: &str,
    bridge: Arc<AudioBridge>,
    token: CancellationToken,
    paused: Arc<AtomicBool>,
) -> Result<(), FeederError> {
    let path = download_attachment(attachment_url, ext, &token).await?;
    let result = feed_pcm_to_bridge(&path, &bridge, &token, &paused).await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

#[cfg(test)]
mod tests {
    use super::split_stereo_frames;

    /// A little-endian byte stream of the f32 sequence 0.0, 1.0, 2.0, …, n-1.
    fn le_stream(n: usize) -> Vec<u8> {
        (0..n).flat_map(|i| (i as f32).to_le_bytes()).collect()
    }

    #[test]
    fn aligned_chunk_passes_through_whole() {
        let mut carry = Vec::new();
        let samples = split_stereo_frames(&mut carry, &le_stream(4));
        assert_eq!(samples, vec![0.0, 1.0, 2.0, 3.0]);
        assert!(carry.is_empty());
    }

    #[test]
    fn tail_is_carried_not_dropped() {
        let mut carry = Vec::new();
        let stream = le_stream(4);
        // First read stops 3 bytes short of the second frame.
        let first = split_stereo_frames(&mut carry, &stream[..13]);
        assert_eq!(first, vec![0.0, 1.0], "only the whole frame is emitted");
        assert_eq!(carry.len(), 5);
        // The next read completes it; nothing lost, order intact.
        let second = split_stereo_frames(&mut carry, &stream[13..]);
        assert_eq!(second, vec![2.0, 3.0]);
        assert!(carry.is_empty());
    }

    #[test]
    fn emits_only_even_sample_counts() {
        // 12 bytes = 3 whole f32s, but an odd sample count would shift L/R at
        // the bridge — one whole frame comes out, one sample stays carried.
        let mut carry = Vec::new();
        let samples = split_stereo_frames(&mut carry, &le_stream(3));
        assert_eq!(samples, vec![0.0, 1.0]);
        assert_eq!(carry.len(), 4);
    }

    #[test]
    fn byte_at_a_time_stream_reassembles_exactly() {
        // The worst pipe-read pattern: every read lands mid-sample. All frames
        // must come out, in order, with every emission frame-even.
        let stream = le_stream(64);
        let mut carry = Vec::new();
        let mut out = Vec::new();
        for b in &stream {
            let chunk = split_stereo_frames(&mut carry, std::slice::from_ref(b));
            assert_eq!(chunk.len() % 2, 0);
            assert!(carry.len() < 8, "carry must stay under one frame");
            out.extend(chunk);
        }
        let expected: Vec<f32> = (0..64).map(|i| i as f32).collect();
        assert_eq!(out, expected);
        assert!(carry.is_empty());
    }
}
