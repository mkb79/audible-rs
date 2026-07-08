//! Widevine device blob (`.wvd`) parsing (AUD-56a).
//!
//! A pywidevine `.wvd` bundles the CDM's RSA private key and its serialized
//! `ClientIdentification` protobuf. We parse the container here; the key and
//! client_id feed the license challenge (built in the client). The private key
//! is a secret — zeroized on drop and never printed.

use std::fmt;

use zeroize::Zeroizing;

/// The device kind recorded in a `.wvd` (`type_` byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    /// `type_ == 1`.
    Chrome,
    /// `type_ == 2` (what Audible's Widevine path uses).
    Android,
    /// Any other value, kept verbatim for forward-compatibility.
    Other(u8),
}

impl From<u8> for DeviceType {
    fn from(value: u8) -> Self {
        match value {
            1 => DeviceType::Chrome,
            2 => DeviceType::Android,
            other => DeviceType::Other(other),
        }
    }
}

/// Errors from parsing a `.wvd` blob.
#[derive(Debug, thiserror::Error)]
pub enum WvdError {
    /// The blob does not start with the `WVD` magic.
    #[error("not a .wvd file (bad magic)")]
    BadMagic,
    /// The version byte is not one we parse.
    #[error("unsupported .wvd version {0} (expected 1 or 2)")]
    Version(u8),
    /// A length prefix ran past the end of the blob.
    #[error(".wvd is truncated")]
    Truncated,
}

/// A parsed Widevine device: its RSA private key (DER) and client id.
///
/// `.wvd` layout (pywidevine v1/v2): `b"WVD"` magic, `version` (u8), `type_`
/// (u8), `security_level` (u8), `flags` (u8), then length-prefixed (`u16`
/// big-endian) `private_key` (RSA PKCS#1 DER) and `client_id`
/// (`ClientIdentification` protobuf). v1 carries a trailing VMP blob we ignore.
pub struct Device {
    device_type: DeviceType,
    security_level: u8,
    /// RSA private key, PKCS#1 DER. Secret: zeroized on drop, never printed.
    private_key_der: Zeroizing<Vec<u8>>,
    /// Serialized `ClientIdentification` protobuf (embedded in the challenge).
    client_id: Vec<u8>,
}

impl Device {
    /// Parses a `.wvd` blob into a device.
    pub fn from_wvd(bytes: &[u8]) -> Result<Self, WvdError> {
        let mut cur = Cursor::new(bytes);
        if cur.take(3)? != b"WVD" {
            return Err(WvdError::BadMagic);
        }
        let version = cur.u8()?;
        if version != 1 && version != 2 {
            return Err(WvdError::Version(version));
        }
        let device_type = DeviceType::from(cur.u8()?);
        let security_level = cur.u8()?;
        let _flags = cur.u8()?;
        let private_key_der = cur.length_prefixed()?.to_vec();
        let client_id = cur.length_prefixed()?.to_vec();
        // v1 has a trailing VMP field after client_id; not needed here.
        Ok(Self {
            device_type,
            security_level,
            private_key_der: Zeroizing::new(private_key_der),
            client_id,
        })
    }

    /// The device kind (`type_`).
    pub fn device_type(&self) -> DeviceType {
        self.device_type
    }

    /// The Widevine security level (L1/L2/L3 → 1/2/3).
    pub fn security_level(&self) -> u8 {
        self.security_level
    }

    /// The RSA private key, PKCS#1 DER (secret).
    pub fn private_key_der(&self) -> &[u8] {
        &self.private_key_der
    }

    /// The serialized `ClientIdentification` protobuf.
    pub fn client_id(&self) -> &[u8] {
        &self.client_id
    }
}

/// Hand-written so the private key never reaches any debug output.
impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("device_type", &self.device_type)
            .field("security_level", &self.security_level)
            .field("private_key_der", &"<redacted>")
            .field("client_id_len", &self.client_id.len())
            .finish()
    }
}

/// Minimal big-endian byte cursor for the fixed `.wvd` layout.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WvdError> {
        let end = self.pos.checked_add(n).ok_or(WvdError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(WvdError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WvdError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WvdError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn length_prefixed(&mut self) -> Result<&'a [u8], WvdError> {
        let len = self.u16()? as usize;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic `.wvd` blob (v2 layout) for the parser tests.
    fn wvd(version: u8, type_: u8, level: u8, key: &[u8], client_id: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"WVD");
        v.push(version);
        v.push(type_);
        v.push(level);
        v.push(0); // flags
        v.extend_from_slice(&(key.len() as u16).to_be_bytes());
        v.extend_from_slice(key);
        v.extend_from_slice(&(client_id.len() as u16).to_be_bytes());
        v.extend_from_slice(client_id);
        v
    }

    #[test]
    fn parses_v2_fields() {
        let blob = wvd(2, 2, 3, b"PRIVATEKEYDER", b"CLIENTIDPROTO");
        let device = Device::from_wvd(&blob).unwrap();
        assert_eq!(device.device_type(), DeviceType::Android);
        assert_eq!(device.security_level(), 3);
        assert_eq!(device.private_key_der(), b"PRIVATEKEYDER");
        assert_eq!(device.client_id(), b"CLIENTIDPROTO");
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        assert!(matches!(Device::from_wvd(b"XXXX"), Err(WvdError::BadMagic)));
        let blob = wvd(9, 2, 3, b"k", b"c");
        assert!(matches!(Device::from_wvd(&blob), Err(WvdError::Version(9))));
    }

    #[test]
    fn rejects_truncated() {
        let blob = wvd(2, 2, 3, b"key", b"cid");
        assert!(matches!(
            Device::from_wvd(&blob[..blob.len() - 2]),
            Err(WvdError::Truncated)
        ));
    }

    #[test]
    fn debug_hides_the_private_key() {
        let blob = wvd(2, 1, 3, b"SECRETKEYBYTES", b"cid");
        let device = Device::from_wvd(&blob).unwrap();
        let rendered = format!("{device:?}");
        assert!(!rendered.contains("SECRETKEYBYTES"));
        assert!(rendered.contains("redacted"));
    }
}
