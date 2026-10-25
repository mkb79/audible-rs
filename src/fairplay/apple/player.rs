//! A small AVPlayer wrapper for FairPlay assets: online (streaming keys) or a
//! downloaded package (stored keys). AVPlayer is main-thread only.

use std::path::Path;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVPlayer, AVPlayerItem, AVPlayerItemStatus, AVPlayerTimeControlStatus, AVURLAsset,
};
use objc2_core_media::CMTime;
use objc2_foundation::{NSString, NSURL};

use super::{KeySession, describe};

/// Where a [`Player`] stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerStatus {
    /// Still loading (keys, playlists).
    Loading,
    /// Ready to play.
    Ready,
    /// Playback failed.
    Failed(String),
}

/// An AVPlayer for one FairPlay asset.
pub struct Player {
    player: Retained<AVPlayer>,
    item: Retained<AVPlayerItem>,
    _asset: Retained<AVURLAsset>,
}

impl Player {
    /// A player for the signed HLS URL of a grant; `keys` (a
    /// [`super::KeyMode::Streaming`] session) answers its key requests.
    pub fn remote(mtm: MainThreadMarker, url: &str, keys: &KeySession) -> Result<Self, String> {
        let url = NSURL::URLWithString(&NSString::from_str(url))
            .ok_or_else(|| "the playlist URL is not a URL".to_owned())?;
        Ok(Self::open(mtm, &url, keys))
    }

    /// A player for a downloaded package; `keys` (a
    /// [`super::KeyMode::Stored`] session) answers with the stored keys.
    pub fn package(mtm: MainThreadMarker, package: &Path, keys: &KeySession) -> Self {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&package.to_string_lossy()));
        Self::open(mtm, &url, keys)
    }

    fn open(mtm: MainThreadMarker, url: &NSURL, keys: &KeySession) -> Self {
        let asset = unsafe { AVURLAsset::URLAssetWithURL_options(url, None) };
        keys.add_recipient(&asset);
        let item = unsafe { AVPlayerItem::playerItemWithAsset(&asset, mtm) };
        let player = unsafe { AVPlayer::playerWithPlayerItem(Some(&item), mtm) };
        Self {
            player,
            item,
            _asset: asset,
        }
    }

    /// Starts or resumes playback.
    pub fn play(&self) {
        unsafe { self.player.play() };
    }

    /// Pauses playback.
    pub fn pause(&self) {
        unsafe { self.player.pause() };
    }

    /// Jumps to `seconds`.
    pub fn seek(&self, seconds: f64) {
        unsafe { self.player.seekToTime(CMTime::with_seconds(seconds, 600)) };
    }

    /// Where the player stands.
    pub fn status(&self) -> PlayerStatus {
        match unsafe { self.item.status() } {
            AVPlayerItemStatus::ReadyToPlay => PlayerStatus::Ready,
            AVPlayerItemStatus::Failed => PlayerStatus::Failed(
                unsafe { self.item.error() }
                    .map(|error| describe(&error))
                    .unwrap_or_else(|| "AVFoundation gave no reason".to_owned()),
            ),
            _ => PlayerStatus::Loading,
        }
    }

    /// The playback position, in seconds.
    pub fn position(&self) -> f64 {
        unsafe { self.player.currentTime().seconds() }
    }

    /// The asset length in seconds, once known.
    pub fn duration(&self) -> Option<f64> {
        let seconds = unsafe { self.item.duration().seconds() };
        (seconds.is_finite() && seconds > 0.0).then_some(seconds)
    }

    /// Whether audio is playing right now.
    pub fn is_playing(&self) -> bool {
        let status = unsafe { self.player.timeControlStatus() };
        status == AVPlayerTimeControlStatus::Playing
    }
}
