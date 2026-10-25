//! The FairPlay content-key session. AVFoundation raises a key request for
//! every `skd://` key an asset needs; [`KeySession`] answers it with a
//! streaming key, a new persistable key, or a stored one.
//!
//! The SPC → CKC exchange is the caller's: the session reports
//! [`KeyEvent::NeedsCkc`] and takes the answer back through
//! [`KeySession::respond`]. Persistable keys come back from `respond` (and
//! as [`KeyEvent::KeyUpdated`] when AVFoundation renews one) for the caller
//! to store; they are DRM material, bound to this Mac, and never logged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class, msg_send};
use objc2_av_foundation::{
    AVContentKeyRequest, AVContentKeyRequestProtocolVersionsKey, AVContentKeyRequestRetryReason,
    AVContentKeyResponse, AVContentKeySession, AVContentKeySessionDelegate,
    AVContentKeySystemFairPlayStreaming, AVPersistableContentKeyRequest, AVURLAsset,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSError, NSNumber, NSObject, NSString, NSURL, ns_string,
};

use super::describe;

/// The FairPlay protocol version the key requests ask for.
const PROTOCOL_VERSION: i32 = 1;

/// How a session answers key requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// A streaming key per request (online playback); nothing is stored.
    Streaming,
    /// A new persistable key per request (downloads), returned by
    /// [`KeySession::respond`] for the caller to store.
    Persist,
    /// The stored persistable keys the session was created with (offline
    /// playback; no network).
    Stored,
}

/// A key request waiting for its CKC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

/// What a session reports. Received through the channel
/// [`KeySession::new`] returns.
pub enum KeyEvent {
    /// A key request needs a CKC: exchange `spc` with the license server
    /// (`fairplay::request_ckc`), then call [`KeySession::respond`] (or
    /// [`KeySession::reject`]).
    NeedsCkc {
        /// The request to answer.
        request: RequestId,
        /// The content id of the requested key.
        content_id: String,
        /// The SPC AVFoundation built.
        spc: Vec<u8>,
    },
    /// AVFoundation renewed a stored persistable key; replace the stored one.
    KeyUpdated {
        /// The content id of the key.
        content_id: String,
        /// The renewed persistable key.
        key: Vec<u8>,
    },
    /// AVFoundation accepted a key.
    Accepted {
        /// The content id of the key, when the request carried one.
        content_id: Option<String>,
    },
    /// A key request failed. With `request` set, the caller should
    /// [`KeySession::reject`] it so it does not linger.
    Failed {
        /// The failed request, when it was waiting for a CKC.
        request: Option<RequestId>,
        /// The content id of the key, when known.
        content_id: Option<String>,
        /// What went wrong.
        error: String,
    },
}

impl std::fmt::Debug for KeyEvent {
    // Without the SPC and key bytes: they are DRM material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyEvent::NeedsCkc {
                request,
                content_id,
                spc,
            } => f
                .debug_struct("NeedsCkc")
                .field("request", request)
                .field("content_id", content_id)
                .field("spc_bytes", &spc.len())
                .finish(),
            KeyEvent::KeyUpdated { content_id, key } => f
                .debug_struct("KeyUpdated")
                .field("content_id", content_id)
                .field("key_bytes", &key.len())
                .finish(),
            KeyEvent::Accepted { content_id } => f
                .debug_struct("Accepted")
                .field("content_id", content_id)
                .finish(),
            KeyEvent::Failed {
                request,
                content_id,
                error,
            } => f
                .debug_struct("Failed")
                .field("request", request)
                .field("content_id", content_id)
                .field("error", error)
                .finish(),
        }
    }
}

/// A request waiting for its CKC.
enum Pending {
    Streaming(Retained<AVContentKeyRequest>),
    Persistable(Retained<AVPersistableContentKeyRequest>),
}

/// The delegate's state. AVFoundation's key types are thread-safe, so the
/// delegate is too, although its callbacks arrive on the main queue.
struct State {
    mode: KeyMode,
    certificate: Vec<u8>,
    stored: HashMap<String, Vec<u8>>,
    events: mpsc::Sender<KeyEvent>,
    pending: Mutex<HashMap<u64, Pending>>,
    next: AtomicU64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AudibleRsFairPlayKeyDelegate"]
    #[ivars = State]
    struct KeyDelegate;

    unsafe impl NSObjectProtocol for KeyDelegate {}

    unsafe impl AVContentKeySessionDelegate for KeyDelegate {
        #[unsafe(method(contentKeySession:didProvideContentKeyRequest:))]
        fn did_provide(&self, _session: &AVContentKeySession, request: &AVContentKeyRequest) {
            self.on_request(request);
        }

        #[unsafe(method(contentKeySession:didProvidePersistableContentKeyRequest:))]
        fn did_provide_persistable(
            &self,
            _session: &AVContentKeySession,
            request: &AVPersistableContentKeyRequest,
        ) {
            self.on_persistable_request(request);
        }

        #[unsafe(method(contentKeySession:contentKeyRequestDidSucceed:))]
        fn did_succeed(&self, _session: &AVContentKeySession, request: &AVContentKeyRequest) {
            self.send(KeyEvent::Accepted {
                content_id: content_id_of(request),
            });
        }

        #[unsafe(method(contentKeySession:contentKeyRequest:didFailWithError:))]
        fn did_fail(
            &self,
            _session: &AVContentKeySession,
            request: &AVContentKeyRequest,
            error: &NSError,
        ) {
            self.send(KeyEvent::Failed {
                request: None,
                content_id: content_id_of(request),
                error: describe(error),
            });
        }

        #[unsafe(method(contentKeySession:shouldRetryContentKeyRequest:reason:))]
        fn should_retry(
            &self,
            _session: &AVContentKeySession,
            _request: &AVContentKeyRequest,
            _reason: &AVContentKeyRequestRetryReason,
        ) -> bool {
            false
        }

        #[unsafe(method(contentKeySession:didUpdatePersistableContentKey:forContentKeyIdentifier:))]
        fn did_update(&self, _session: &AVContentKeySession, key: &NSData, identifier: &AnyObject) {
            match identifier_string(identifier)
                .as_deref()
                .and_then(crate::fairplay::hls::content_id)
            {
                Some(content_id) => self.send(KeyEvent::KeyUpdated {
                    content_id: content_id.to_owned(),
                    key: key.to_vec(),
                }),
                None => self.send(KeyEvent::Failed {
                    request: None,
                    content_id: None,
                    error: "a renewed key without a content id".into(),
                }),
            }
        }
    }
);

impl KeyDelegate {
    fn new(state: State) -> Retained<Self> {
        let this = Self::alloc().set_ivars(state);
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: KeyEvent) {
        // A dropped receiver means nobody listens any more; nothing to do.
        let _ = self.ivars().events.send(event);
    }

    fn on_request(&self, request: &AVContentKeyRequest) {
        if self.ivars().mode == KeyMode::Streaming {
            self.request_ckc(request, Pending::Streaming(request.retain()));
            return;
        }
        // Ask for the persistable variant: that is what yields a key blob
        // that can be stored and handed back later without the network.
        if let Err(error) =
            unsafe { request.respondByRequestingPersistableContentKeyRequestAndReturnError() }
        {
            self.send(KeyEvent::Failed {
                request: None,
                content_id: content_id_of(request),
                error: describe(&error),
            });
        }
    }

    fn on_persistable_request(&self, request: &AVPersistableContentKeyRequest) {
        let state = self.ivars();
        if state.mode != KeyMode::Stored {
            self.request_ckc(request, Pending::Persistable(request.retain()));
            return;
        }
        let content_id = content_id_of(request);
        match content_id.as_ref().and_then(|id| state.stored.get(id)) {
            Some(key) => {
                let key = NSData::with_bytes(key);
                let response = unsafe {
                    AVContentKeyResponse::contentKeyResponseWithFairPlayStreamingKeyResponseData(
                        &key,
                    )
                };
                unsafe { request.processContentKeyResponse(&response) };
            }
            None => {
                unsafe { request.processContentKeyResponseError(&key_error()) };
                self.send(KeyEvent::Failed {
                    request: None,
                    content_id,
                    error: "no stored key for this content id".into(),
                });
            }
        }
    }

    /// Asks AVFoundation for the SPC and reports it as [`KeyEvent::NeedsCkc`].
    fn request_ckc(&self, request: &AVContentKeyRequest, pending: Pending) {
        let state = self.ivars();
        let Some(content_id) = content_id_of(request) else {
            unsafe { request.processContentKeyResponseError(&key_error()) };
            self.send(KeyEvent::Failed {
                request: None,
                content_id: None,
                error: "a key request without an skd:// content id".into(),
            });
            return;
        };
        let id = state.next.fetch_add(1, Ordering::Relaxed);
        state
            .pending
            .lock()
            .expect("key requests lock")
            .insert(id, pending);

        let content_id_data = NSData::with_bytes(content_id.as_bytes());
        let versions = NSArray::from_retained_slice(&[NSNumber::numberWithInt(PROTOCOL_VERSION)]);
        let options: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[unsafe { AVContentKeyRequestProtocolVersionsKey }],
            &[versions.as_ref()],
        );
        // The handler runs on an AVFoundation queue: it only passes plain
        // bytes on, through the (thread-safe) channel.
        let events = state.events.clone();
        let handler = RcBlock::new(move |spc: *mut NSData, error: *mut NSError| {
            let spc = unsafe { spc.as_ref() }.map(NSData::to_vec);
            let error = unsafe { error.as_ref() }.map(describe);
            let event = match (spc, error) {
                (_, Some(error)) => KeyEvent::Failed {
                    request: Some(RequestId(id)),
                    content_id: Some(content_id.clone()),
                    error: format!("SPC: {error}"),
                },
                (Some(spc), None) if !spc.is_empty() => KeyEvent::NeedsCkc {
                    request: RequestId(id),
                    content_id: content_id.clone(),
                    spc,
                },
                _ => KeyEvent::Failed {
                    request: Some(RequestId(id)),
                    content_id: Some(content_id.clone()),
                    error: "AVFoundation returned no SPC".into(),
                },
            };
            let _ = events.send(event);
        });
        unsafe {
            request
                .makeStreamingContentKeyRequestDataForApp_contentIdentifier_options_completionHandler(
                    &NSData::with_bytes(&state.certificate),
                    Some(&content_id_data),
                    Some(&options),
                    &handler,
                );
        }
    }
}

/// A FairPlay content-key session. Its callbacks arrive on the main queue,
/// so the main run loop must run (see [`super::pump`]).
pub struct KeySession {
    session: Retained<AVContentKeySession>,
    delegate: Retained<KeyDelegate>,
}

impl KeySession {
    /// Creates a session answering in `mode`, with the provider
    /// `certificate` (from the grant, or `fairplay::request_certificate`).
    /// `stored` maps content ids to stored persistable keys and only matters
    /// for [`KeyMode::Stored`]. Events arrive through the returned channel
    /// while the main queue runs.
    pub fn new(
        mode: KeyMode,
        certificate: &[u8],
        stored: HashMap<String, Vec<u8>>,
    ) -> (Self, mpsc::Receiver<KeyEvent>) {
        let (events, receiver) = mpsc::channel();
        let delegate = KeyDelegate::new(State {
            mode,
            certificate: certificate.to_vec(),
            stored,
            events,
            pending: Mutex::default(),
            next: AtomicU64::new(1),
        });
        let session = unsafe {
            AVContentKeySession::contentKeySessionWithKeySystem(AVContentKeySystemFairPlayStreaming)
        };
        unsafe {
            session.setDelegate_queue(
                Some(ProtocolObject::from_ref(&*delegate)),
                Some(DispatchQueue::main()),
            );
        }
        (Self { session, delegate }, receiver)
    }

    /// Lets the session answer the key requests of `asset` (call before the
    /// asset is played or downloaded).
    pub fn add_recipient(&self, asset: &AVURLAsset) {
        unsafe {
            asset.resourceLoader().setPreloadsEligibleContentKeys(true);
            self.session
                .addContentKeyRecipient(ProtocolObject::from_ref(asset));
        }
    }

    /// Raises a key request for `key_uri` (`skd://…`) without waiting for
    /// playback to need it — to fetch a key up front.
    pub fn request_key(&self, key_uri: &str) {
        let identifier = NSString::from_str(key_uri);
        unsafe {
            self.session
                .processContentKeyRequestWithIdentifier_initializationData_options(
                    Some(identifier.as_ref()),
                    None,
                    None,
                );
        }
    }

    /// Answers a [`KeyEvent::NeedsCkc`] with the CKC from the license server.
    /// For a persistable request it returns the new persistable key, which
    /// the caller stores (it is already handed to AVFoundation).
    pub fn respond(&self, request: RequestId, ckc: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let pending = self.take(request)?;
        let ckc = NSData::with_bytes(ckc);
        match pending {
            Pending::Streaming(request) => {
                // Streaming: the CKC itself is the key response.
                let response = unsafe {
                    AVContentKeyResponse::contentKeyResponseWithFairPlayStreamingKeyResponseData(
                        &ckc,
                    )
                };
                unsafe { request.processContentKeyResponse(&response) };
                Ok(None)
            }
            Pending::Persistable(request) => {
                let key = match unsafe {
                    request.persistableContentKeyFromKeyVendorResponse_options_error(&ckc, None)
                } {
                    Ok(key) if key.length() > 0 => key,
                    Ok(_) => {
                        unsafe { request.processContentKeyResponseError(&key_error()) };
                        return Err("AVFoundation returned an empty persistable key".into());
                    }
                    Err(error) => {
                        unsafe { request.processContentKeyResponseError(&error) };
                        return Err(describe(&error));
                    }
                };
                let response = unsafe {
                    AVContentKeyResponse::contentKeyResponseWithFairPlayStreamingKeyResponseData(
                        &key,
                    )
                };
                unsafe { request.processContentKeyResponse(&response) };
                Ok(Some(key.to_vec()))
            }
        }
    }

    /// Fails a waiting request (the CKC could not be obtained).
    pub fn reject(&self, request: RequestId) {
        if let Ok(pending) = self.take(request) {
            let request: &AVContentKeyRequest = match &pending {
                Pending::Streaming(request) => request,
                Pending::Persistable(request) => request,
            };
            unsafe { request.processContentKeyResponseError(&key_error()) };
        }
    }

    fn take(&self, request: RequestId) -> Result<Pending, String> {
        self.delegate
            .ivars()
            .pending
            .lock()
            .expect("key requests lock")
            .remove(&request.0)
            .ok_or_else(|| "unknown or already answered key request".to_owned())
    }
}

/// The content id of a key request: the host of its `skd://` identifier.
fn content_id_of(request: &AVContentKeyRequest) -> Option<String> {
    let identifier = unsafe { request.identifier() }?;
    let uri = identifier_string(&identifier)?;
    crate::fairplay::hls::content_id(&uri).map(str::to_owned)
}

fn identifier_string(identifier: &AnyObject) -> Option<String> {
    if let Some(text) = identifier.downcast_ref::<NSString>() {
        return Some(text.to_string());
    }
    identifier
        .downcast_ref::<NSURL>()
        .and_then(|url| url.absoluteString())
        .map(|text| text.to_string())
}

/// The error a request is failed with when no key can be supplied.
fn key_error() -> Retained<NSError> {
    NSError::new(1, ns_string!("AudibleRsFairPlay"))
}
