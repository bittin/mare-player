// SPDX-License-Identifier: GPL-3.0-only

//! The TIDAL client identity Maré Player presents — one place, one answer.
//!
//! TIDAL decides the stream ceiling from the **OAuth client** a token was
//! minted for, independently of the subscription. So an unofficial client has
//! to pick one of TIDAL's first-party clients and then stay consistent with
//! it: the login flow it supports, and the identity playback events claim.
//!
//! ## Why this client
//!
//! Maré signs in with the **PKCE** flow, which is what makes `HI_RES_LOSSLESS`
//! reachable at all. Measured against the live API with an account whose
//! `highestSoundQuality` is `HI_RES`, on a track whose `mediaMetadata.tags`
//! carry `HIRES_LOSSLESS`:
//!
//! * Device-code clients — TIDAL's TV and Android-Automotive head units,
//!   including the `4N3n6Q1x95LL5K7p` client tidlers uses by default — answer
//!   `playbackinfopostpaywall?audioquality=HI_RES_LOSSLESS` with `LOSSLESS`,
//!   `bitDepth` 16, `sampleRate` 44100. That is the downgrade
//!   [`get_track_playback_url`](super::client::TidalAppClient::get_track_playback_url)
//!   reports.
//! * The PKCE client below — TIDAL's Android **mobile** client — is granted the
//!   hi-res FLAC tier, and issues refresh tokens, so sessions still survive
//!   reboots without re-login.
//!
//! The entitlement rides on the client id; both flows authenticate with an id
//! *and* secret, which tidlers embeds.
//!
//! tidlers keeps this client's credentials in its `PkceConfig::try_default`;
//! the *public* id is duplicated here only so [`verify`] can check the session
//! we actually hold against the identity we report. The secret stays there.
//!
//! ## Why the play reporter needs it
//!
//! `playback_session` events carry a `client` object describing the app that
//! played the track, and it has to describe the same client the token belongs
//! to. Maré reports with tidlers' `EventClient::tidal_android()`, which tidlers
//! keeps next to those same PKCE credentials: an Android phone client, so
//! plays reach Recently Played.

use tracing::warn;

/// The OAuth client Maré Player authenticates as, and whose identity it
/// reports with playback events.
pub struct ClientIdentity {
    /// Public OAuth client id. Must match tidlers' PKCE config.
    pub client_id: &'static str,
}

/// TIDAL's Android mobile client — see the module docs for why this one.
pub const TIDAL_CLIENT: ClientIdentity = ClientIdentity { client_id: "6BDSRdpK9hqEBTgU" };

/// Warn when the session we hold isn't the client the constants above describe
/// — e.g. because a tidlers upgrade changed its embedded PKCE client.
///
/// The session itself is unaffected; what drifts is the identity we report
/// alongside playback events, so this logs rather than failing the login.
pub fn verify(session_client_id: &str) {
    if session_client_id != TIDAL_CLIENT.client_id {
        warn!(
            expected = TIDAL_CLIENT.client_id,
            actual = session_client_id,
            "TIDAL client id differs from the one mare reports as; playback events may be attributed to the wrong client",
        );
    }
}
