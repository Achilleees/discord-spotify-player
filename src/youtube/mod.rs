pub mod metadata;
pub mod feeder;

pub use metadata::{YoutubeMetadata, fetch_youtube_metadata, YoutubeError};
pub use feeder::{feed_youtube_to_bridge, feed_file_to_bridge, FeederError};
