use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album_art_url: Option<String>,
    pub spotify_track_id: String,
}

#[derive(Deserialize)]
struct SpotifyTrackResponse {
    name: String,
    artists: Vec<SpotifyArtist>,
    album: SpotifyAlbum,
}

#[derive(Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Deserialize)]
struct SpotifyAlbum {
    images: Vec<SpotifyImage>,
}

#[derive(Deserialize)]
struct SpotifyImage {
    url: String,
    #[allow(dead_code)]
    width: Option<u32>,
    #[allow(dead_code)]
    height: Option<u32>,
}

pub async fn fetch_track_metadata(
    track_id: &str,
    access_token: &str,
) -> Option<TrackMetadata> {
    let client = reqwest::Client::new();
    let url = format!("https://api.spotify.com/v1/tracks/{}", track_id);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(status = resp.status().as_u16(), "failed to fetch track metadata");
        return None;
    }

    let track: SpotifyTrackResponse = resp.json().await.ok()?;
    let artist = track
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let album_art_url = track.album.images.first().map(|img| img.url.clone());

    Some(TrackMetadata {
        title: track.name,
        artist,
        album_art_url,
        spotify_track_id: track_id.to_string(),
    })
}
