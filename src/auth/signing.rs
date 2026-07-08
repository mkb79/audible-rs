//! RSA-SHA256 request signing exactly as the Audible API expects it.
//!
//! The signed data is `method\npath\ntimestamp\nbody\nadp_token` (the path
//! includes the query string), signed with the device's RSA private key
//! using PKCS#1 v1.5 and SHA-256. The result is carried in the
//! `x-adp-token`, `x-adp-alg` and `x-adp-signature` headers, mirroring
//! `audible.auth.sign_request` from the Python reference implementation.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use sha2::Sha256;

/// Value of the `x-adp-alg` header.
pub const ADP_ALG: &str = "SHA256withRSA:1.0";

/// Header carrying the adp_token.
pub const HEADER_ADP_TOKEN: &str = "x-adp-token";
/// Header carrying the signing algorithm identifier.
pub const HEADER_ADP_ALG: &str = "x-adp-alg";
/// Header carrying `<base64 signature>:<timestamp>`.
pub const HEADER_ADP_SIGNATURE: &str = "x-adp-signature";

/// Errors raised while constructing a [`RequestSigner`].
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    /// The device private key is not a valid PKCS#1 PEM document.
    #[error("invalid device private key: {0}")]
    InvalidKey(#[from] rsa::pkcs1::Error),
}

/// The three headers that authenticate a signed request.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    /// Value for [`HEADER_ADP_TOKEN`].
    pub adp_token: String,
    /// Value for [`HEADER_ADP_ALG`].
    pub alg: &'static str,
    /// Value for [`HEADER_ADP_SIGNATURE`]: `<base64 signature>:<timestamp>`.
    pub signature: String,
}

impl fmt::Debug for SignedHeaders {
    // The adp_token is a credential; never print header values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedHeaders").finish_non_exhaustive()
    }
}

/// Signs API requests with a device's RSA private key.
///
/// Parse the key once per account session and reuse the signer; signing
/// itself is cheap compared to PEM parsing.
pub struct RequestSigner {
    signing_key: SigningKey<Sha256>,
    adp_token: String,
}

impl fmt::Debug for RequestSigner {
    // Holds key material; never derive Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestSigner").finish_non_exhaustive()
    }
}

impl RequestSigner {
    /// Creates a signer from a PKCS#1 PEM private key (the
    /// `device_private_key` of an auth file) and the matching adp_token.
    pub fn new(private_key_pem: &str, adp_token: impl Into<String>) -> Result<Self, SigningError> {
        let key = RsaPrivateKey::from_pkcs1_pem(private_key_pem)?;
        Ok(Self {
            signing_key: SigningKey::new(key),
            adp_token: adp_token.into(),
        })
    }

    /// Signs a request with the current time.
    ///
    /// `path_and_query` is the URL path including the query string, e.g.
    /// `/1.0/library?num_results=999`.
    pub fn sign_request(&self, method: &str, path_and_query: &str, body: &[u8]) -> SignedHeaders {
        self.sign_request_at(method, path_and_query, body, &signing_timestamp_now())
    }

    /// Signs a request with an explicit timestamp (exposed for golden
    /// tests; production code uses [`Self::sign_request`]).
    pub fn sign_request_at(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        timestamp: &str,
    ) -> SignedHeaders {
        let mut data = Vec::with_capacity(
            method.len()
                + path_and_query.len()
                + timestamp.len()
                + body.len()
                + self.adp_token.len()
                + 4,
        );
        data.extend_from_slice(method.as_bytes());
        data.push(b'\n');
        data.extend_from_slice(path_and_query.as_bytes());
        data.push(b'\n');
        data.extend_from_slice(timestamp.as_bytes());
        data.push(b'\n');
        data.extend_from_slice(body);
        data.push(b'\n');
        data.extend_from_slice(self.adp_token.as_bytes());

        // PKCS#1 v1.5 signing is deterministic and infallible.
        let signature = self.signing_key.sign(&data);
        let encoded = BASE64.encode(signature.to_bytes());

        SignedHeaders {
            adp_token: self.adp_token.clone(),
            alg: ADP_ALG,
            signature: format!("{encoded}:{timestamp}"),
        }
    }
}

/// Current UTC time in the exact format of the Python reference:
/// `datetime.now(timezone.utc).isoformat("T") + "Z"`, e.g.
/// `2026-01-02T03:04:05.678901+00:00Z`.
///
/// Unlike Python's `isoformat`, the microseconds part is always emitted,
/// even when it is zero; the API accepts both.
fn signing_timestamp_now() -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]+00:00Z"
    );
    time::OffsetDateTime::now_utc()
        .format(format)
        .expect("formatting a UTC timestamp with a const format never fails")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_python_isoformat_shape() {
        let ts = signing_timestamp_now();
        // e.g. 2026-01-02T03:04:05.678901+00:00Z
        assert_eq!(ts.len(), 33);
        assert!(ts.ends_with("+00:00Z"));
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], ".");
    }

    #[test]
    fn debug_output_hides_credentials() {
        let headers = SignedHeaders {
            adp_token: "secret".into(),
            alg: ADP_ALG,
            signature: "sig:ts".into(),
        };
        let debug = format!("{headers:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("sig:ts"));
    }
}
