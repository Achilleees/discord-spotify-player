//! An operator-curated, bounded catalogue of local soundboard clips.
//!
//! Discord only sees stable IDs and labels. Paths and decoder diagnostics stay
//! local; selection never accepts a URL, arbitrary path or ffmpeg argument.

use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};

const MAX_CLIPS: usize = 128;
const MANIFEST_LIMIT: usize = 64 * 1024;
const FILE_LIMIT: usize = 20 * 1024 * 1024;
const PCM_LIMIT: usize = 44_100 * 2 * 4 * 15;
// A small bounded tail distinguishes oversized audio from an exact 15-second clip.
const PCM_OUTPUT_LIMIT: usize = PCM_LIMIT + 44_100 * 2 * 4 / 20;
const STDERR_LIMIT: usize = 32 * 1024;
const DECODE_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_PATH: &str = "soundboard catalogue contains an invalid local clip path";
const FILE_UNAVAILABLE: &str =
    "That sound is unavailable. Ask the server owner to check its local audio file.";
const DECODE_FAILED: &str =
    "That sound could not be decoded. Ask the server owner to check its audio file.";

#[derive(Clone)]
pub(crate) struct Clip {
    pub id: String,
    pub label: String,
    file: PathBuf,
}

#[derive(Default)]
pub(crate) struct Catalogue {
    root: PathBuf,
    clips: Vec<Clip>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    clips: Vec<ManifestClip>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestClip {
    id: String,
    label: String,
    file: String,
}

impl Catalogue {
    /// Load without creating directories or files. An absent catalogue is an
    /// empty soundboard; a present but invalid catalogue is a configuration error.
    pub(crate) fn load(root: &Path) -> Result<Self, String> {
        if network_root(root) {
            return Err("soundboard directory must be local".into());
        }
        let root = match fs::canonicalize(root) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    root: root.to_path_buf(),
                    clips: Vec::new(),
                });
            }
            Err(_) => return Err("soundboard directory could not be opened".into()),
        };
        if !root.is_dir() {
            return Err("soundboard directory is not a directory".into());
        }
        let manifest = match fs::symlink_metadata(root.join("catalogue.json")) {
            Ok(metadata) => {
                if is_link(&metadata) || !metadata.is_file() {
                    return Err("soundboard catalogue must be a regular local file".into());
                }
                if metadata.len() > MANIFEST_LIMIT as u64 {
                    return Err("soundboard catalogue exceeds 64 KiB".into());
                }
                let path = checked_file(&root, Path::new("catalogue.json"), MANIFEST_LIMIT)
                    .map_err(|_| "soundboard catalogue could not be opened")?;
                let bytes = read_file(&path, MANIFEST_LIMIT)
                    .map_err(|_| "soundboard catalogue could not be read within 64 KiB")?;
                serde_json::from_slice::<Manifest>(&bytes)
                    .map_err(|_| "soundboard catalogue must contain valid clips JSON")?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    root,
                    clips: Vec::new(),
                });
            }
            Err(_) => return Err("soundboard catalogue could not be opened".into()),
        };
        if manifest.clips.len() > MAX_CLIPS {
            return Err("soundboard catalogue may contain at most 128 clips".into());
        }
        let mut ids = HashSet::new();
        let mut clips = Vec::with_capacity(manifest.clips.len());
        for clip in manifest.clips {
            if !valid_id(&clip.id) || !ids.insert(clip.id.clone()) {
                return Err("soundboard clip IDs must be unique, using 1-32 letters, digits, underscores or hyphens".into());
            }
            let label = clip.label.trim();
            if label.is_empty()
                || label.chars().count() > 60
                || clip.label.chars().any(char::is_control)
            {
                return Err("soundboard clip labels must contain 1-60 printable characters".into());
            }
            let file = relative_file(&clip.file)?;
            checked_file(&root, &file, FILE_LIMIT).map_err(|_| {
                "soundboard clips must be regular files inside its directory, at most 20 MiB each"
            })?;
            clips.push(Clip {
                id: clip.id,
                label: label.to_owned(),
                file,
            });
        }
        Ok(Self { root, clips })
    }

    pub(crate) fn clips(&self) -> &[Clip] {
        &self.clips
    }

    /// Decode clips of at most 15 seconds into stereo 44.1 kHz f32le PCM. The caller owns
    /// cancellation: dropping this future kills ffmpeg and releases its permit.
    pub(crate) async fn decode(&self, id: &str) -> Result<Vec<u8>, String> {
        let clip = self
            .clips
            .iter()
            .find(|clip| clip.id == id)
            .ok_or("That sound is no longer in the catalogue. Open /soundboard again.")?;
        static PERMITS: OnceLock<Semaphore> = OnceLock::new();
        let _permit = PERMITS
            .get_or_init(|| Semaphore::new(2))
            .try_acquire()
            .map_err(|_| "The soundboard is busy preparing clips. Try again in a moment.")?;

        let operation = async {
            // Check again on selection: the catalogue may have been edited or a
            // previously valid file replaced since this process started.
            let path =
                checked_file(&self.root, &clip.file, FILE_LIMIT).map_err(|_| FILE_UNAVAILABLE)?;
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|_| FILE_UNAVAILABLE)?;
            let metadata = file.metadata().await.map_err(|_| FILE_UNAVAILABLE)?;
            if !metadata.is_file() || metadata.len() > FILE_LIMIT as u64 {
                return Err(FILE_UNAVAILABLE.into());
            }
            let bytes = read_capped(file, FILE_LIMIT)
                .await
                .map_err(|_| FILE_UNAVAILABLE)?;
            if checked_file(&self.root, &clip.file, FILE_LIMIT).map_err(|_| FILE_UNAVAILABLE)?
                != path
            {
                return Err(FILE_UNAVAILABLE.into());
            }
            decode_bytes(decoder_command(), bytes).await
        };
        tokio::time::timeout(DECODE_TIMEOUT, operation)
            .await
            .map_err(|_| {
                "Preparing that sound took too long. Try a shorter audio file.".to_owned()
            })?
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Validate both separator conventions on every host, so a catalogue reviewed
/// on Linux cannot become a drive/UNC/alternate-stream path when run on Windows.
fn relative_file(value: &str) -> Result<PathBuf, String> {
    if value.is_empty()
        || value.len() > 512
        || value.chars().any(char::is_control)
        || value.contains([':', '*', '?', '"', '<', '>', '|'])
    {
        return Err(INVALID_PATH.into());
    }
    let mut result = PathBuf::new();
    for part in value.split(['/', '\\']) {
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with(['.', ' '])
            || windows_device_name(part)
        {
            return Err(INVALID_PATH.into());
        }
        result.push(part);
    }
    if result
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(INVALID_PATH.into());
    }
    Ok(result)
}

fn windows_device_name(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or(part)
        .trim_matches(' ')
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn network_root(root: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        root.components().any(|component| {
            matches!(component, Component::Prefix(prefix)
                if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        })
    }
    #[cfg(not(windows))]
    {
        let value = root.to_string_lossy();
        value.starts_with("//") || value.starts_with("\\\\")
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and other reparse points, not only symlinks.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn checked_file(root: &Path, relative: &Path, limit: usize) -> io::Result<PathBuf> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid local sound file");
    let mut path = root.to_path_buf();
    for part in relative.components() {
        if !matches!(part, Component::Normal(_)) {
            return Err(invalid());
        }
        path.push(part);
        if is_link(&fs::symlink_metadata(&path)?) {
            return Err(invalid());
        }
    }
    let path = fs::canonicalize(path)?;
    if !path.starts_with(root) {
        return Err(invalid());
    }
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(invalid());
    }
    Ok(path)
}

fn read_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sound file too large",
        ));
    }
    Ok(bytes)
}

async fn read_capped(reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sound output too large",
        ));
    }
    Ok(bytes)
}

fn decoder_command() -> Command {
    let mut command = Command::new("ffmpeg");
    command.env_remove("FFREPORT").args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "pipe",
        "-format_whitelist",
        "aac,aiff,flac,matroska,webm,mov,mp3,ogg,wav",
        "-probesize",
        "1048576",
        "-analyzeduration",
        "3000000",
        "-i",
        "pipe:0",
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-t",
        "15.05",
        "-ac",
        "2",
        "-ar",
        "44100",
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "pipe:1",
    ]);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
}

async fn decode_bytes(mut command: Command, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            "The soundboard decoder is unavailable. Ask the server owner to install ffmpeg."
        })?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let input = async move {
        let result = stdin.write_all(&bytes).await;
        // The decoder stops reading after the duration probe. Exit status and
        // PCM length still decide whether this clip is valid or too long.
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::BrokenPipe {
                return Err(error);
            }
        }
        drop(stdin);
        Ok(())
    };
    let (status, _, pcm, _stderr) = tokio::try_join!(
        child.wait(),
        input,
        read_capped(stdout, PCM_OUTPUT_LIMIT),
        read_capped(stderr, STDERR_LIMIT),
    )
    .map_err(|_| DECODE_FAILED)?;
    if status.success() && pcm.len() > PCM_LIMIT {
        return Err("Soundboard clips must be 15 seconds or shorter. Ask the server owner to shorten this clip.".into());
    }
    if !status.success() || !valid_pcm(&pcm) {
        return Err(DECODE_FAILED.into());
    }
    Ok(pcm)
}

fn valid_pcm(pcm: &[u8]) -> bool {
    !pcm.is_empty()
        && pcm.len() <= PCM_LIMIT
        && pcm.len().is_multiple_of(8)
        && pcm
            .as_chunks::<4>()
            .0
            .iter()
            .all(|sample| f32::from_le_bytes(*sample).is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("soundboard-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn manifest(&self, clips: serde_json::Value) {
            fs::write(
                self.0.join("catalogue.json"),
                serde_json::to_vec(&serde_json::json!({ "clips": clips })).unwrap(),
            )
            .unwrap();
        }

        fn clip(&self, file: &str, bytes: &[u8]) {
            fs::write(self.0.join(file), bytes).unwrap();
            self.manifest(
                serde_json::json!([{ "id": "party", "label": "Party time", "file": file }]),
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // A uniquely created test directory is the only deletion target.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_catalogue_is_empty_and_does_not_create_files() {
        let fixture = Fixture::new();
        assert!(Catalogue::load(&fixture.0).unwrap().clips().is_empty());
        let missing = fixture.0.join("missing");
        assert!(Catalogue::load(&missing).unwrap().clips().is_empty());
        assert!(!missing.exists());
        assert!(!fixture.0.join("catalogue.json").exists());
    }

    #[test]
    fn catalogue_preserves_order_and_only_exposes_labels_and_ids() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("second.wav"), b"local").unwrap();
        fs::create_dir(fixture.0.join("sub")).unwrap();
        fs::write(fixture.0.join("sub/first.wav"), b"local").unwrap();
        fixture.manifest(serde_json::json!([
            {"id":"second", "label":"Second", "file":"second.wav"},
            {"id":"first", "label":"  First 🎺  ", "file":"sub\\first.wav"}
        ]));
        let catalogue = Catalogue::load(&fixture.0).unwrap();
        assert_eq!(catalogue.clips()[0].id, "second");
        assert_eq!(catalogue.clips()[1].label, "First 🎺");
    }

    #[test]
    fn rejects_portable_traversal_drives_network_streams_and_device_names() {
        for path in [
            "",
            "/tmp/sound.wav",
            "../secret.wav",
            "sub/../sound.wav",
            ".\\sound.wav",
            "C:\\sound.wav",
            "C:sound.wav",
            "\\\\host\\share\\sound.wav",
            "//host/sound.wav",
            "sound.wav:secret",
            "sub//sound.wav",
            "sub/",
            "sound.wav.",
            "sound.wav ",
            "NUL",
            "con.wav",
            "sub/COM1.mp3",
            "LPT9.wav",
            "COM¹.wav",
            "sound\n.wav",
            "https://example.test/sound.wav",
            "pipe:0",
            "a?.wav",
        ] {
            assert!(relative_file(path).is_err(), "accepted {path:?}");
        }
        assert_eq!(
            relative_file("nested/party.wav").unwrap(),
            Path::new("nested").join("party.wav")
        );
        assert!(relative_file("comic.wav").is_ok());
    }

    #[test]
    fn rejects_duplicate_ids_invalid_labels_and_unknown_manifest_fields() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("sound.wav"), b"sound").unwrap();
        let clip = serde_json::json!({"id":"clip", "label":"Clip", "file":"sound.wav"});
        fixture.manifest(serde_json::json!([clip, clip]));
        assert!(Catalogue::load(&fixture.0).is_err());
        for (id, label) in [
            ("bad:id", "Clip"),
            ("", "Clip"),
            ("clip", "\nClip"),
            ("clip", "  "),
        ] {
            fixture
                .manifest(serde_json::json!([{ "id": id, "label": label, "file": "sound.wav" }]));
            assert!(Catalogue::load(&fixture.0).is_err());
        }
        fixture.manifest(
            serde_json::json!([{ "id": "clip", "label": "a".repeat(61), "file": "sound.wav" }]),
        );
        assert!(Catalogue::load(&fixture.0).is_err());
        fs::write(
            fixture.0.join("catalogue.json"),
            br#"{"clips":[],"unknown":true}"#,
        )
        .unwrap();
        assert!(Catalogue::load(&fixture.0).is_err());
    }

    #[test]
    fn rejects_oversized_manifests_files_and_catalogues() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("catalogue.json"),
            vec![b' '; MANIFEST_LIMIT + 1],
        )
        .unwrap();
        assert!(Catalogue::load(&fixture.0).is_err());
        fixture.clip("sound.wav", b"sound");
        fs::OpenOptions::new()
            .write(true)
            .open(fixture.0.join("sound.wav"))
            .unwrap()
            .set_len(FILE_LIMIT as u64 + 1)
            .unwrap();
        assert!(Catalogue::load(&fixture.0).is_err());
        fs::write(fixture.0.join("sound.wav"), b"sound").unwrap();
        let clips: Vec<_> = (0..=MAX_CLIPS).map(|i| serde_json::json!({ "id": format!("clip{i}"), "label": "Clip", "file": "sound.wav" })).collect();
        fixture.manifest(serde_json::json!(clips));
        assert!(Catalogue::load(&fixture.0).is_err());
    }

    #[test]
    fn accepts_exact_catalogue_file_and_manifest_limits() {
        let fixture = Fixture::new();
        fs::File::create(fixture.0.join("sound.wav"))
            .unwrap()
            .set_len(FILE_LIMIT as u64)
            .unwrap();
        let clips: Vec<_> = (0..MAX_CLIPS)
            .map(|i| {
                serde_json::json!({
                    "id": format!("{i:032}"),
                    "label": "x".repeat(60),
                    "file": "sound.wav"
                })
            })
            .collect();
        fixture.manifest(serde_json::json!(clips));
        assert_eq!(
            Catalogue::load(&fixture.0).unwrap().clips().len(),
            MAX_CLIPS
        );
        assert!(!valid_id(&"x".repeat(33)));
        let mut manifest = br#"{"clips":[]}"#.to_vec();
        manifest.resize(MANIFEST_LIMIT, b' ');
        fs::write(fixture.0.join("catalogue.json"), manifest).unwrap();
        assert!(Catalogue::load(&fixture.0).unwrap().clips().is_empty());
    }

    #[tokio::test]
    async fn selection_rechecks_deleted_or_oversized_files_before_spawning() {
        let fixture = Fixture::new();
        fixture.clip("sound.wav", b"sound");
        let catalogue = Catalogue::load(&fixture.0).unwrap();
        fs::remove_file(fixture.0.join("sound.wav")).unwrap();
        assert_eq!(
            catalogue.decode("party").await.unwrap_err(),
            FILE_UNAVAILABLE
        );
        let file = fs::File::create(fixture.0.join("sound.wav")).unwrap();
        file.set_len(FILE_LIMIT as u64 + 1).unwrap();
        assert_eq!(
            catalogue.decode("party").await.unwrap_err(),
            FILE_UNAVAILABLE
        );
        assert!(catalogue
            .decode("../secret")
            .await
            .unwrap_err()
            .contains("no longer"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_manifest_and_clip_symlinks_at_load_and_selection() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fixture.clip("sound.wav", b"sound");
        outside.clip("outside.wav", b"private outside bytes");
        let catalogue = Catalogue::load(&fixture.0).unwrap();
        fs::remove_file(fixture.0.join("sound.wav")).unwrap();
        symlink(outside.0.join("outside.wav"), fixture.0.join("sound.wav")).unwrap();
        assert!(Catalogue::load(&fixture.0).is_err());
        assert_eq!(
            catalogue.decode("party").await.unwrap_err(),
            FILE_UNAVAILABLE
        );
        fs::remove_file(fixture.0.join("catalogue.json")).unwrap();
        symlink(
            outside.0.join("catalogue.json"),
            fixture.0.join("catalogue.json"),
        )
        .unwrap();
        assert!(Catalogue::load(&fixture.0).is_err());
    }

    #[tokio::test]
    async fn bounded_pipe_rejects_first_extra_byte_without_waiting_for_eof() {
        let (reader, mut writer) = tokio::io::duplex(32);
        writer.write_all(b"too many bytes").await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), read_capped(reader, 3))
            .await
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_capped(&b"abc"[..], 3).await.unwrap(), b"abc");
    }

    #[test]
    fn pcm_must_be_finite_complete_stereo_frames() {
        assert!(valid_pcm(&[0; 8]));
        assert!(!valid_pcm(&[]));
        assert!(!valid_pcm(&[0; 4]));
        let invalid: Vec<_> = [f32::NAN, 0.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert!(!valid_pcm(&invalid));
        assert!(!valid_pcm(&vec![0; PCM_LIMIT + 8]));
    }

    fn wav(seconds: usize) -> Vec<u8> {
        // Small, synthetic mono 8 kHz PCM input exercises real channel/rate conversion.
        let samples = seconds * 8_000;
        let data_size = (samples * 2) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        for i in 0..samples {
            let sample: i16 = if i % 16 < 8 { 4_000 } else { -4_000 };
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[tokio::test]
    async fn real_ffmpeg_converts_rejects_long_clips_and_playlists_when_available() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let short = decode_bytes(decoder_command(), wav(1)).await.unwrap();
        assert_eq!(short.len(), 44_100 * 2 * 4);
        assert!(short.iter().any(|byte| *byte != 0));
        let longest = decode_bytes(decoder_command(), wav(15)).await.unwrap();
        assert_eq!(longest.len(), PCM_LIMIT);
        let too_long = decode_bytes(decoder_command(), wav(16)).await.unwrap_err();
        assert!(too_long.contains("15 seconds or shorter"));
        let playlist =
            b"#EXTM3U\n#EXTINF:1,not a clip\nfile:///private-file-that-must-not-be-opened.wav\n";
        let error = decode_bytes(decoder_command(), playlist.to_vec())
            .await
            .unwrap_err();
        assert_eq!(error, DECODE_FAILED);
        assert!(!error.contains("private-file"));
    }

    fn fixture_command(fixture: &Fixture, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "soundboard::tests::decoder_child_fixture",
                "--nocapture",
            ])
            .env("SOUNDBOARD_TEST_CHILD", &fixture.0)
            .env("SOUNDBOARD_TEST_MODE", mode);
        command
    }

    async fn wait_for_child(fixture: &Fixture) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fixture.0.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelling_decode_kills_its_child() {
        let fixture = Fixture::new();
        let command = fixture_command(&fixture, "wait");
        let task = tokio::spawn(decode_bytes(command, Vec::new()));
        wait_for_child(&fixture).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !fixture.0.join("survived").exists(),
            "cancelled decoder kept running"
        );
    }

    #[tokio::test]
    async fn timing_out_decode_kills_its_child() {
        let fixture = Fixture::new();
        let command = fixture_command(&fixture, "wait");
        {
            let operation = decode_bytes(command, Vec::new());
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => panic!("decoder unexpectedly finished: {result:?}"),
                () = wait_for_child(&fixture) => {}
            }
            assert!(tokio::time::timeout(Duration::from_millis(20), operation)
                .await
                .is_err());
            // Drop the underlying future as the production timeout does.
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !fixture.0.join("survived").exists(),
            "timed-out decoder kept running"
        );
    }

    #[tokio::test]
    async fn decoder_output_budgets_kill_children_and_hide_diagnostics() {
        for mode in ["stdout", "stderr", "failure"] {
            let fixture = Fixture::new();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                decode_bytes(fixture_command(&fixture, mode), Vec::new()),
            )
            .await
            .unwrap();
            assert_eq!(result.unwrap_err(), DECODE_FAILED);
            tokio::time::sleep(Duration::from_millis(900)).await;
            assert!(
                !fixture.0.join("survived").exists(),
                "over-budget child kept running"
            );
        }
    }

    #[test]
    fn decoder_child_fixture() {
        use std::io::Write;
        let Some(root) = std::env::var_os("SOUNDBOARD_TEST_CHILD") else {
            return;
        };
        let root = PathBuf::from(root);
        fs::write(root.join("started"), b"started").unwrap();
        match std::env::var("SOUNDBOARD_TEST_MODE").unwrap().as_str() {
            "stdout" => {
                let _ = io::stdout().write_all(&vec![0; PCM_OUTPUT_LIMIT + 1]);
            }
            "stderr" => {
                let _ = io::stderr().write_all(&vec![0; STDERR_LIMIT + 1]);
            }
            "failure" => {
                let _ = io::stderr().write_all(b"synthetic private path or metadata");
                std::process::exit(1);
            }
            "wait" => {}
            _ => panic!("unknown decoder fixture mode"),
        }
        std::thread::sleep(Duration::from_millis(750));
        fs::write(root.join("survived"), b"survived").unwrap();
    }
}
