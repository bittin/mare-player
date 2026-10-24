// SPDX-License-Identifier: GPL-3.0-only

//! Decide which listens to report to TIDAL, so they count for the user's
//! Recently Played, recommendations and artist royalties.
//!
//! Sending is tidlers' job: [`PlaybackReporter`](tidlers::client::api::events::PlaybackReporter)
//! queues finished listens and posts them to TIDAL's event producer from a
//! background task, so playback never waits on the network. This module
//! keeps what is Maré's policy rather than TIDAL's protocol: tracking the
//! listen in progress and only reporting it once it counts as a play.
//!
//! ## Recently Played vs aggregate counters
//!
//! `source_type` / `source_id` decide where a play shows up:
//! * `ALBUM` / `PLAYLIST` / `MIX` / `ARTIST` container listens surface in
//!   Recently Played and credit royalties.
//! * `TRACK` listens count toward "Most Listened" aggregates but **don't**
//!   surface in Recently Played.

use std::time::{SystemTime, UNIX_EPOCH};

use tidlers::client::models::events::PlaybackSession;
use uuid::Uuid;

/// Bookkeeping for the currently-playing track's listen.
///
/// The playback handler opens one of these when a track starts, updates
/// `last_position_s` on every tick, and turns it into a [`PlaybackSession`]
/// via [`InProgressPlay::finalize`] when the track ends.
#[derive(Debug, Clone)]
pub struct InProgressPlay {
    pub session_id: String,
    pub track_id: String,
    /// e.g. `"HI_RES_LOSSLESS"`, `"LOSSLESS"`, `"HIGH"`, `"LOW"`.
    pub quality: String,
    /// Container that started this listen: `"ALBUM"`, `"PLAYLIST"`,
    /// `"MIX"`, `"ARTIST"`, or `"TRACK"` when there is no container.
    pub source_type: Option<String>,
    pub source_id: Option<String>,
    pub start_ts_ms: u64,
    pub start_position_s: f64,
    /// High-water mark of the position seen during this listen.  Track
    /// position can drop briefly at end-of-track when the engine resets
    /// before the next-track transition; using the max preserves the
    /// real listened duration.
    pub last_position_s: f64,
    /// Total track duration in seconds (for the 50%-listened threshold).
    pub duration_s: f64,
}

impl InProgressPlay {
    /// Open a new listen of `track_id`.
    pub fn open(
        track_id: String,
        quality: String,
        source_type: Option<String>,
        source_id: Option<String>,
        start_position_s: f64,
        duration_s: f64,
    ) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            track_id,
            quality,
            source_type,
            source_id,
            start_ts_ms: now_ms(),
            start_position_s,
            last_position_s: start_position_s,
            duration_s,
        }
    }

    /// Bump `last_position_s` to `pos_s` if it's higher than the current
    /// high-water mark.  Cheap to call on every playback tick.
    pub fn observe_position(&mut self, pos_s: f64) {
        if pos_s > self.last_position_s {
            self.last_position_s = pos_s;
        }
    }

    /// True if the listen exceeded 30 seconds OR 50% of the track
    /// duration, whichever comes first.  Matches the threshold Spotify
    /// and Apple Music use — short skips don't generate spurious plays.
    pub fn meets_threshold(&self) -> bool {
        let listened = (self.last_position_s - self.start_position_s).max(0.0);
        listened >= 30.0 || (self.duration_s > 0.0 && listened >= self.duration_s / 2.0)
    }

    /// Consume `self` and produce the finished listen, ending now.
    pub fn finalize(self) -> PlaybackSession {
        PlaybackSession {
            playback_session_id: self.session_id,
            track_id: self.track_id,
            quality: self.quality,
            source_type: self.source_type,
            source_id: self.source_id,
            start_timestamp_ms: self.start_ts_ms,
            end_timestamp_ms: now_ms(),
            start_position_s: self.start_position_s,
            end_position_s: self.last_position_s,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_progress_threshold_30s_listen() {
        let mut p = InProgressPlay::open("42".into(), "LOSSLESS".into(), None, None, 0.0, 300.0);
        assert!(!p.meets_threshold());
        p.observe_position(29.5);
        assert!(!p.meets_threshold());
        p.observe_position(30.0);
        assert!(p.meets_threshold());
    }

    #[test]
    fn in_progress_threshold_half_duration() {
        // Short track: 50% = 10s, less than the 30s cutoff.
        let mut p = InProgressPlay::open("42".into(), "LOSSLESS".into(), None, None, 0.0, 20.0);
        p.observe_position(9.0);
        assert!(!p.meets_threshold());
        p.observe_position(10.0);
        assert!(p.meets_threshold());
    }

    #[test]
    fn in_progress_observe_is_high_water_mark() {
        // Engine may briefly snap position back to 0 right before a track
        // switch.  observe_position must not regress.
        let mut p = InProgressPlay::open("42".into(), "LOSSLESS".into(), None, None, 0.0, 300.0);
        p.observe_position(200.0);
        p.observe_position(0.0);
        assert_eq!(p.last_position_s, 200.0);
    }

    #[test]
    fn finalize_reports_the_listened_span_and_source() {
        let mut p = InProgressPlay::open("42".into(), "LOSSLESS".into(), Some("ALBUM".into()), Some("999".into()), 5.0, 300.0);
        p.observe_position(185.0);
        let session_id = p.session_id.clone();
        let start_ts_ms = p.start_ts_ms;
        let s = p.finalize();
        assert_eq!(s.playback_session_id, session_id);
        assert_eq!(s.track_id, "42");
        assert_eq!(s.quality, "LOSSLESS");
        assert_eq!(s.source_type.as_deref(), Some("ALBUM"));
        assert_eq!(s.source_id.as_deref(), Some("999"));
        assert_eq!(s.start_timestamp_ms, start_ts_ms);
        assert!(s.end_timestamp_ms >= start_ts_ms);
        assert_eq!(s.start_position_s, 5.0);
        assert_eq!(s.end_position_s, 185.0);
    }
}
