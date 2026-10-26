//! Terminal rendering of `rustunes pull`.
use anyhow::Result;

use crate::engine::{PullResult, PulledTrack};

/// Unbound tracks listed in full; the rest are counted.
const LISTED: usize = 25;

fn describe(track: &PulledTrack) -> String {
    let name = match (track.artist.is_empty(), track.title.is_empty()) {
        (_, true) => "(untitled)".to_owned(),
        (true, false) => track.title.clone(),
        (false, false) => format!("{} - {}", track.artist, track.title),
    };
    match &track.reason {
        Some(reason) => format!("{} {name}: {reason}", track.track_id),
        None => format!("{} {name}", track.track_id),
    }
}
