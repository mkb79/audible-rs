//! The macOS half of FairPlay: AVFoundation through the objc2 bindings, no
//! Swift. [`KeySession`] answers AVFoundation's key requests,
//! [`PackageDownload`] downloads a title into an AVFoundation package, and
//! [`Player`] plays one online or offline.
//!
//! Everything here is bound to the main thread: the delegates run on the main
//! queue — the arrangement the spike proved on a real Mac (AUD-321). A Mac app
//! drives that queue with its own run loop; a command-line tool calls
//! [`pump`] in a loop. The network half of the key exchange (SPC → CKC) is the
//! caller's, so no AVFoundation queue ever waits on the network and any async
//! runtime fits.

mod download;
mod keys;
mod player;

use std::cell::OnceCell;
use std::ptr::NonNull;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSError, NSRunLoop, NSTimer};

pub use download::{DownloadEvent, PackageDownload, protect_package};
pub use keys::{KeyEvent, KeyMode, KeySession, RequestId};
pub use player::{Player, PlayerStatus};

thread_local! {
    /// Keeps the main run loop alive: without an attached source it returns
    /// at once, and the main dispatch queue alone does not count as one.
    static KEEP_ALIVE: OnceCell<Retained<NSTimer>> = const { OnceCell::new() };
}

/// Runs the main run loop once, for at most `slice`, delivering the main
/// queue's callbacks (key requests, download events). A command-line tool
/// calls this in a loop while it waits; a Mac app's run loop does it already.
pub fn pump(_mtm: MainThreadMarker, slice: Duration) {
    KEEP_ALIVE.with(|timer| {
        timer.get_or_init(|| {
            let block = RcBlock::new(|_timer: NonNull<NSTimer>| {});
            unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(3600.0, true, &block) }
        });
    });
    let until = NSDate::dateWithTimeIntervalSinceNow(slice.as_secs_f64());
    NSRunLoop::mainRunLoop().runMode_beforeDate(unsafe { NSDefaultRunLoopMode }, &until);
}

/// An `NSError` as `domain code: description`, for messages.
pub(crate) fn describe(error: &NSError) -> String {
    format!(
        "{} {}: {}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    )
}
