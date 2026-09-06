//! Bounded metadata-only yt-dlp execution shared by links and text search.

use super::metadata::{classify_ytdlp_stderr, YoutubeError};
use std::{io, process::Stdio, sync::OnceLock, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const STDOUT_LIMIT: usize = 2 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

async fn read_capped(reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "probe output limit exceeded",
        ));
    }
    Ok(bytes)
}

/// `input` must be a validated media URL or a locally constructed ytsearch query.
pub(super) async fn run(input: &str) -> Result<String, YoutubeError> {
    static PERMITS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let _permit = PERMITS
        .get_or_init(|| tokio::sync::Semaphore::new(3))
        .try_acquire()
        .map_err(|_| YoutubeError::Busy)?;

    let operation = async {
        let mut command = tokio::process::Command::new("yt-dlp");
        command.args([
            "--ignore-config",
            "--no-warnings",
            "--dump-json",
            "--no-playlist",
            "--flat-playlist",
            "--socket-timeout",
            "10",
            "--retries",
            "1",
            "--extractor-retries",
            "1",
            "--remote-components",
            "ejs:github",
        ]);
        command.arg("--cache-dir").arg(super::extractor_cache_path());
        let cookies = super::cookies_path();
        if std::path::Path::new(&cookies).is_file() {
            command.args(["--cookies", &cookies]);
        }
        let mut child = command
            .args(["--", input])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        // Read both pipes while waiting: neither a full stderr pipe nor a
        // runaway JSON response may stall the process or exhaust memory.
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            read_capped(stdout, STDOUT_LIMIT),
            read_capped(stderr, STDERR_LIMIT),
        )?;
        Ok::<_, io::Error>((status, stdout, stderr))
    };
    let (status, stdout, stderr) = match tokio::time::timeout(PROBE_TIMEOUT, operation).await {
        Err(_) => return Err(YoutubeError::Timeout),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "media metadata probe failed");
            return Err(if error.kind() == io::ErrorKind::InvalidData {
                YoutubeError::ResponseTooLarge
            } else {
                YoutubeError::Network("couldn't look up that track — try again".into())
            });
        }
        Ok(Ok(output)) => output,
    };
    if !status.success() {
        return Err(classify_ytdlp_stderr(&String::from_utf8_lossy(&stderr)));
    }
    String::from_utf8(stdout).map_err(|_| YoutubeError::Parse("invalid metadata response".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_at_the_limit_is_preserved() {
        assert_eq!(read_capped(&b"track"[..], 5).await.unwrap(), b"track");
    }

    #[tokio::test]
    async fn oversized_output_is_rejected_without_waiting_for_eof() {
        let (reader, mut writer) = tokio::io::duplex(32);
        use tokio::io::AsyncWriteExt;
        writer.write_all(b"too much metadata").await.unwrap();
        // Keep the writer open: rejecting the first extra byte must finish
        // even if a child would otherwise continue writing forever.
        let error = tokio::time::timeout(Duration::from_secs(1), read_capped(reader, 4))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
