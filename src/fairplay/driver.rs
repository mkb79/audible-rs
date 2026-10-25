//! A blocking facade over the AVFoundation half, for callers that are not
//! built around a run loop — the `audible fairplay` commands. A Mac app with
//! its own run loop uses the [`super::apple`] types directly instead.
//!
//! Each call blocks the calling thread — which must be the main thread — and
//! runs the main run loop in short slices, answering the session's events in
//! between. SPCs go to the caller's `ckc` exchange (which may block on the
//! network); everything else is reported through the event callback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;

use super::apple::{
    DownloadEvent, KeyEvent, KeyMode, KeySession, PackageDownload, Player, PlayerStatus, pump,
};

/// One run-loop slice between two looks at the events.
const SLICE: Duration = Duration::from_millis(100);
/// How long a finished transfer may still wait for its key.
const KEY_GRACE: Duration = Duration::from_secs(30);

/// Exchanges an SPC for a CKC with the license server:
/// `(content id, SPC) → CKC`.
pub type CkcExchange<'a> = dyn FnMut(&str, &[u8]) -> Result<Vec<u8>, String> + 'a;

/// A package download.
pub struct DownloadJob<'a> {
    /// The grant's signed HLS master playlist URL.
    pub hls_url: &'a str,
    /// The name the system shows for the download.
    pub title: &'a str,
    /// The provider certificate.
    pub certificate: &'a [u8],
}

/// A finished download.
pub struct Downloaded {
    /// Where AVFoundation put the package. Apple's documentation expects it
    /// to stay there; `audible fairplay` moves it into its store all the
    /// same, which the spike showed to keep it playable.
    pub package: PathBuf,
    /// The persistable keys by content id — DRM material, to be stored
    /// owner-only and never logged.
    pub keys: HashMap<String, Vec<u8>>,
}

/// Progress of a download.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadProgress {
    /// Where the package goes.
    Location(PathBuf),
    /// How much is done, `0.0..=1.0`.
    Fraction(f64),
    /// AVFoundation accepted a key.
    KeyAccepted,
}

/// What to play.
pub enum PlaySource<'a> {
    /// Online: the grant's signed HLS URL, with a streaming key per request.
    Stream {
        /// The signed HLS master playlist URL.
        hls_url: &'a str,
        /// The provider certificate.
        certificate: &'a [u8],
    },
    /// Offline: a downloaded package with its stored persistable keys.
    Package {
        /// The package (`.movpkg`).
        path: &'a Path,
        /// The stored keys by content id.
        keys: &'a HashMap<String, Vec<u8>>,
    },
}

/// How to play.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Playback {
    /// Where to start, in seconds.
    pub start: f64,
    /// Start exactly at `start` instead of the segment boundary before it.
    pub exact: bool,
    /// How long to play, in seconds; `None` plays to the end.
    pub duration: Option<f64>,
}

/// What playback reports.
pub enum PlayEvent {
    /// The asset is ready; its length in seconds, when known.
    Ready {
        /// The asset length.
        duration: Option<f64>,
    },
    /// The playback position, in seconds (about once a second).
    Position(f64),
    /// AVFoundation renewed a stored key: replace the stored one.
    KeyUpdated {
        /// The content id of the key.
        content_id: String,
        /// The renewed persistable key (DRM material).
        key: Vec<u8>,
    },
    /// Playback reached the end.
    Ended,
}

fn main_thread() -> Result<MainThreadMarker, String> {
    MainThreadMarker::new().ok_or_else(|| "FairPlay has to run on the main thread".to_owned())
}

/// Answers the session's queued events: SPCs go through `ckc`, new and
/// renewed persistable keys to `on_key`.
fn answer_keys(
    keys: &KeySession,
    events: &mpsc::Receiver<KeyEvent>,
    ckc: &mut CkcExchange<'_>,
    on_key: &mut dyn FnMut(String, Vec<u8>),
    on_accept: &mut dyn FnMut(),
) -> Result<(), String> {
    while let Ok(event) = events.try_recv() {
        match event {
            KeyEvent::NeedsCkc {
                request,
                content_id,
                spc,
            } => match ckc(&content_id, &spc) {
                Ok(answer) => {
                    if let Some(key) = keys.respond(request, &answer)? {
                        on_key(content_id, key);
                    }
                }
                Err(error) => {
                    keys.reject(request);
                    return Err(format!("license server: {error}"));
                }
            },
            KeyEvent::KeyUpdated { content_id, key } => on_key(content_id, key),
            KeyEvent::Accepted { .. } => on_accept(),
            KeyEvent::Failed { request, error, .. } => {
                if let Some(request) = request {
                    keys.reject(request);
                }
                return Err(format!("key request failed: {error}"));
            }
        }
    }
    Ok(())
}

/// Downloads a FairPlay package with persistable keys. `stop` is polled
/// while it runs; once it returns `true` the download is cancelled.
pub fn download(
    job: &DownloadJob<'_>,
    ckc: &mut CkcExchange<'_>,
    progress: &mut dyn FnMut(DownloadProgress),
    stop: &dyn Fn() -> bool,
) -> Result<Downloaded, String> {
    let mtm = main_thread()?;
    let (keys, key_events) = KeySession::new(KeyMode::Persist, job.certificate, HashMap::new());
    let (download, events) = PackageDownload::start(job.hls_url, job.title, &keys)?;
    let mut location = None;
    let result = drive_download(
        mtm,
        &keys,
        &key_events,
        &download,
        &events,
        ckc,
        progress,
        stop,
        &mut location,
    );
    if result.is_err() {
        download.cancel();
        // A partial package would linger in the system cache, unrecorded.
        if let Some(partial) =
            location.filter(|path| path.extension().is_some_and(|e| e == "movpkg"))
        {
            let _ = std::fs::remove_dir_all(partial);
        }
    }
    let (package, keys) = result?;
    Ok(Downloaded { package, keys })
}

#[allow(clippy::too_many_arguments)]
fn drive_download(
    mtm: MainThreadMarker,
    keys: &KeySession,
    key_events: &mpsc::Receiver<KeyEvent>,
    download: &PackageDownload,
    events: &mpsc::Receiver<DownloadEvent>,
    ckc: &mut CkcExchange<'_>,
    progress: &mut dyn FnMut(DownloadProgress),
    stop: &dyn Fn() -> bool,
    location: &mut Option<PathBuf>,
) -> Result<(PathBuf, HashMap<String, Vec<u8>>), String> {
    let mut stored = HashMap::new();
    let mut finished_at: Option<Instant> = None;
    let mut reported = -1.0;
    loop {
        pump(mtm, SLICE);
        if stop() {
            return Err("interrupted".to_owned());
        }
        answer_keys(
            keys,
            key_events,
            ckc,
            &mut |content_id, key| {
                stored.insert(content_id, key);
            },
            &mut || progress(DownloadProgress::KeyAccepted),
        )?;
        while let Ok(event) = events.try_recv() {
            match event {
                DownloadEvent::Location(path) => {
                    progress(DownloadProgress::Location(path.clone()));
                    *location = Some(path);
                }
                DownloadEvent::Finished => {
                    finished_at.get_or_insert_with(Instant::now);
                }
                DownloadEvent::Failed(error) => return Err(format!("download: {error}")),
            }
        }
        let fraction = download.progress();
        if fraction - reported >= 0.005 {
            reported = fraction;
            progress(DownloadProgress::Fraction(fraction));
        }
        if let Some(at) = finished_at {
            // The transfer can end before the key exchange does.
            if !stored.is_empty() {
                break;
            }
            if at.elapsed() > KEY_GRACE {
                return Err("the download finished without a FairPlay key".to_owned());
            }
        }
    }
    let package = location
        .clone()
        .ok_or_else(|| "AVFoundation reported no package location".to_owned())?;
    if download.playable_offline() == Some(false) {
        return Err("AVFoundation does not consider the package playable offline".to_owned());
    }
    Ok((package, stored))
}

/// Plays a FairPlay asset until it ends, `playback.duration` has passed, or
/// `stop` returns `true`.
pub fn play(
    source: PlaySource<'_>,
    playback: &Playback,
    ckc: &mut CkcExchange<'_>,
    events: &mut dyn FnMut(PlayEvent),
    stop: &dyn Fn() -> bool,
) -> Result<(), String> {
    let mtm = main_thread()?;
    let (keys, key_events, player) = match source {
        PlaySource::Stream {
            hls_url,
            certificate,
        } => {
            let (keys, key_events) =
                KeySession::new(KeyMode::Streaming, certificate, HashMap::new());
            let player = Player::remote(mtm, hls_url, &keys)?;
            (keys, key_events, player)
        }
        PlaySource::Package { path, keys: stored } => {
            let (keys, key_events) = KeySession::new(KeyMode::Stored, &[], stored.clone());
            let player = Player::package(mtm, path, &keys);
            (keys, key_events, player)
        }
    };
    player.play();
    let mut ready = false;
    let mut reported = f64::NEG_INFINITY;
    loop {
        pump(mtm, SLICE);
        if stop() {
            player.pause();
            return Ok(());
        }
        answer_keys(
            &keys,
            &key_events,
            ckc,
            &mut |content_id, key| events(PlayEvent::KeyUpdated { content_id, key }),
            &mut || {},
        )?;
        match player.status() {
            PlayerStatus::Failed(error) => return Err(format!("playback: {error}")),
            PlayerStatus::Loading => continue,
            PlayerStatus::Ready => {}
        }
        if !ready {
            ready = true;
            if playback.start > 0.0 {
                if playback.exact {
                    player.seek_exact(playback.start);
                } else {
                    player.seek(playback.start);
                }
            }
            events(PlayEvent::Ready {
                duration: player.duration(),
            });
        }
        let position = player.position();
        if (position - reported).abs() >= 1.0 {
            reported = position;
            events(PlayEvent::Position(position));
        }
        if playback
            .duration
            .is_some_and(|limit| position - playback.start >= limit)
        {
            player.pause();
            return Ok(());
        }
        if player
            .duration()
            .is_some_and(|total| position >= total - 0.5)
            && !player.is_playing()
        {
            events(PlayEvent::Ended);
            return Ok(());
        }
    }
}
