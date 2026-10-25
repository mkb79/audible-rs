//! The FairPlay package download: AVFoundation downloads the HLS asset into
//! an AVFoundation package (`.movpkg`) through a background
//! `AVAssetDownloadURLSession`, asking the [`KeySession`] for persistable keys
//! on the way.
//!
//! AVFoundation picks the package location and reports it through
//! `willDownloadToURL`; the location is meant to be kept, not moved (Apple:
//! save the URL for later `AVAsset`s). For a tool without an app container
//! that is under `~/Library/Caches/com.apple.nsurlsessiond/Downloads/`. An app
//! that keeps it there can mark it important with [`protect_package`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAssetDownloadConfiguration, AVAssetDownloadDelegate, AVAssetDownloadStorageManager,
    AVAssetDownloadTask, AVAssetDownloadURLSession,
    AVAssetDownloadedAssetEvictionPriorityImportant, AVMutableAssetDownloadStorageManagementPolicy,
    AVURLAsset,
};
use objc2_foundation::{
    NSBundle, NSDate, NSError, NSObject, NSOperationQueue, NSString, NSURL, NSURLSession,
    NSURLSessionConfiguration, NSURLSessionDelegate, NSURLSessionTask, NSURLSessionTaskDelegate,
};

use super::{KeySession, describe};

/// Gives every background session of this process its own identifier.
static SESSIONS: AtomicU64 = AtomicU64::new(1);

/// What a download reports. Received through the channel
/// [`PackageDownload::start`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEvent {
    /// Where AVFoundation puts the package. Keep this path: the package
    /// stays there.
    Location(PathBuf),
    /// The download completed.
    Finished,
    /// The download failed.
    Failed(String),
}

struct State {
    events: mpsc::Sender<DownloadEvent>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AudibleRsFairPlayDownloadDelegate"]
    #[ivars = State]
    struct DownloadDelegate;

    unsafe impl NSObjectProtocol for DownloadDelegate {}

    unsafe impl NSURLSessionDelegate for DownloadDelegate {}

    unsafe impl NSURLSessionTaskDelegate for DownloadDelegate {
        #[unsafe(method(URLSession:task:didCompleteWithError:))]
        fn did_complete(
            &self,
            _session: &NSURLSession,
            _task: &NSURLSessionTask,
            error: Option<&NSError>,
        ) {
            self.send(match error {
                Some(error) => DownloadEvent::Failed(describe(error)),
                None => DownloadEvent::Finished,
            });
        }
    }

    unsafe impl AVAssetDownloadDelegate for DownloadDelegate {
        #[unsafe(method(URLSession:assetDownloadTask:willDownloadToURL:))]
        fn will_download(
            &self,
            _session: &NSURLSession,
            _task: &AVAssetDownloadTask,
            location: &NSURL,
        ) {
            match location.path() {
                Some(path) => self.send(DownloadEvent::Location(PathBuf::from(path.to_string()))),
                None => self.send(DownloadEvent::Failed(
                    "AVFoundation reported a download location without a path".into(),
                )),
            }
        }
    }
);

impl DownloadDelegate {
    fn new(state: State) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: DownloadEvent) {
        let _ = self.ivars().events.send(event);
    }
}

/// A running package download. Its callbacks arrive on the main queue, so
/// the main run loop must run (see [`super::pump`]).
pub struct PackageDownload {
    session: Retained<AVAssetDownloadURLSession>,
    task: Retained<AVAssetDownloadTask>,
    asset: Retained<AVURLAsset>,
    _delegate: Retained<DownloadDelegate>,
}

impl PackageDownload {
    /// Starts downloading the HLS asset at `url` (the grant's signed master
    /// playlist). `keys` (a [`super::KeyMode::Persist`] session) answers the
    /// key requests. `title` names the download for the system. Events
    /// arrive through the returned channel while the main queue runs.
    pub fn start(
        url: &str,
        title: &str,
        keys: &KeySession,
    ) -> Result<(Self, mpsc::Receiver<DownloadEvent>), String> {
        let url = NSURL::URLWithString(&NSString::from_str(url))
            .ok_or_else(|| "the playlist URL is not a URL".to_owned())?;
        let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
        keys.add_recipient(&asset);

        let (events, receiver) = mpsc::channel();
        let delegate = DownloadDelegate::new(State { events });
        let identifier = format!(
            "audible-rs.fairplay.{}.{}",
            std::process::id(),
            SESSIONS.fetch_add(1, Ordering::Relaxed)
        );
        let configuration = NSURLSessionConfiguration::backgroundSessionConfigurationWithIdentifier(
            &NSString::from_str(&identifier),
        );
        configuration.setDiscretionary(false);
        let session = unsafe {
            AVAssetDownloadURLSession::sessionWithConfiguration_assetDownloadDelegate_delegateQueue(
                &configuration,
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(&NSOperationQueue::mainQueue()),
            )
        };
        let download = unsafe {
            AVAssetDownloadConfiguration::downloadConfigurationWithAsset_title(
                &asset,
                &NSString::from_str(title),
            )
        };
        let task = unsafe { session.assetDownloadTaskWithConfiguration(&download) };
        task.resume();
        Ok((
            Self {
                session,
                task,
                asset,
                _delegate: delegate,
            },
            receiver,
        ))
    }

    /// How much is done, `0.0..=1.0`.
    pub fn progress(&self) -> f64 {
        self.task.progress().fractionCompleted()
    }

    /// Whether AVFoundation considers the downloaded asset playable offline.
    pub fn playable_offline(&self) -> Option<bool> {
        unsafe { self.asset.assetCache() }.map(|cache| unsafe { cache.isPlayableOffline() })
    }

    /// Stops the download. A partial package may stay behind at the reported
    /// location.
    pub fn cancel(&self) {
        self.task.cancel();
    }
}

impl Drop for PackageDownload {
    fn drop(&mut self) {
        // A background session lives on in nsurlsessiond until invalidated.
        self.session.finishTasksAndInvalidate();
    }
}

/// Marks the package at `location` as important for the system's storage
/// management, so it is among the last to be purged when space runs low;
/// `expires` adds an expiration date (e.g. the license's access expiry).
/// Returns `false` where it cannot: without the storage manager, or without
/// a bundle identifier — the manager keys its policies on the app and, for a
/// plain command-line tool, raises an exception instead of an error (seen on
/// macOS 27), which would abort the process.
pub fn protect_package(location: &Path, expires: Option<SystemTime>) -> bool {
    if AnyClass::get(c"AVAssetDownloadStorageManager").is_none()
        || NSBundle::mainBundle().bundleIdentifier().is_none()
    {
        return false;
    }
    let policy = unsafe { AVMutableAssetDownloadStorageManagementPolicy::new() };
    unsafe { policy.setPriority(AVAssetDownloadedAssetEvictionPriorityImportant) };
    if let Some(seconds) = expires
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs_f64())
    {
        unsafe { policy.setExpirationDate(&NSDate::dateWithTimeIntervalSince1970(seconds)) };
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&location.to_string_lossy()));
    unsafe {
        AVAssetDownloadStorageManager::sharedDownloadStorageManager()
            .setStorageManagementPolicy_forURL(&policy, &url);
    }
    true
}
