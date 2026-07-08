//! Device profiles for `account login` (the OAuth + `/auth/register` flow).
//!
//! A small catalog of the official apps' device identities, plus a freshly
//! generated serial per login. The constants are captured from live app
//! registrations (iPhone verified end-to-end; Android best-effort from the
//! reference implementation, used by the Widevine path — see AUD-56).

use std::str::FromStr;

use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;

/// Which device profile to register as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// Apple iPhone (the default).
    IPhone,
    /// Android — the registration type that unlocks Widevine (AUD-56).
    Android,
}

/// Static identity of a device profile: the fields the register payload and
/// the OAuth URL need. Captured from live app traffic.
struct DeviceProfile {
    device_type: &'static str,
    device_model: &'static str,
    os_version: &'static str,
    app_name: &'static str,
    app_version: &'static str,
    software_version: &'static str,
    device_name: &'static str,
    /// OAuth `assoc_handle` family → `amzn_audible_<family>_<cc>`
    /// (`ios`; `android_experiment`).
    assoc_handle_family: &'static str,
    /// OAuth `pageId` family → `amzn_audible_<family>_<cc>`
    /// (`ios_v2_light`; `android_aui_v2_dark`).
    page_id_family: &'static str,
    /// OAuth `assoc_handle` family for the username (pre-merger) login →
    /// `amzn_audible_<family>_<cc>` (`ios_lap`; `android_experiment_lap`).
    username_assoc_handle_family: &'static str,
    /// OAuth `pageId` for the username (pre-merger) login: the suffix after
    /// `amzn_audible_`, **without** a `<cc>` (`ios_privatepool`;
    /// `android_privatepool`).
    username_page_id: &'static str,
    /// `registration_data.domain` (`Device` for the iOS app, `DeviceLegacy`
    /// for the Android app).
    registration_domain: &'static str,
}

/// iPhone profile — verified against a live iOS registration.
const IPHONE: DeviceProfile = DeviceProfile {
    device_type: "A2CZJZGLK2JJVM",
    device_model: "iPhone",
    os_version: "26.2",
    app_name: "Audible",
    app_version: "4.60",
    software_version: "46000856",
    device_name: "%FIRST_NAME%%FIRST_NAME_POSSESSIVE_STRING%%DUPE_STRATEGY_1ST%Audible für iPhone",
    assoc_handle_family: "ios",
    page_id_family: "ios_v2_light",
    username_assoc_handle_family: "ios_lap",
    username_page_id: "ios_privatepool",
    registration_domain: "Device",
};

/// Android profile — best-effort from the reference implementation; the
/// registration specifics are verified when the Widevine work (AUD-56) lands.
const ANDROID: DeviceProfile = DeviceProfile {
    device_type: "A10KISP2GWF0E4",
    device_model: "OnePlus8",
    os_version: "30",
    app_name: "com.audible.application",
    app_version: "160008",
    software_version: "130050002",
    device_name: "%FIRST_NAME%%FIRST_NAME_POSSESSIVE_STRING%%DUPE_STRATEGY_1ST%Audible for Android",
    assoc_handle_family: "android_experiment",
    page_id_family: "android_aui_v2_dark",
    // Best-effort by analogy with the iOS `lap`/`privatepool` variant — the
    // reference hardcodes iOS for username login, so the Android values are
    // unverified and may need adjusting if pre-merger Android login fails.
    username_assoc_handle_family: "android_experiment_lap",
    username_page_id: "android_privatepool",
    registration_domain: "DeviceLegacy",
};

impl DeviceKind {
    /// The CLI default when `--device` is omitted. **Change this one line to
    /// switch the default device.**
    pub const DEFAULT: DeviceKind = DeviceKind::IPhone;

    fn profile(self) -> &'static DeviceProfile {
        match self {
            DeviceKind::IPhone => &IPHONE,
            DeviceKind::Android => &ANDROID,
        }
    }

    /// The Amazon device type id (`X-Device-Type-Id`, part of the `client_id`).
    pub fn device_type(self) -> &'static str {
        self.profile().device_type
    }
}

impl FromStr for DeviceKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "iphone" | "ios" => Ok(DeviceKind::IPhone),
            "android" => Ok(DeviceKind::Android),
            other => Err(format!(
                "unknown device {other:?} (expected iphone|android)"
            )),
        }
    }
}

/// A device instance for one login: a profile plus a freshly generated serial.
pub struct Device {
    kind: DeviceKind,
    serial: String,
}

impl Device {
    /// Creates a device with a fresh random serial (the app's 40-char
    /// alphanumeric format).
    pub fn generate(kind: DeviceKind) -> Self {
        Self {
            kind,
            serial: random_serial(),
        }
    }

    /// The device serial (sent in `registration_data` and baked into the
    /// `client_id`).
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// The Amazon device type id.
    pub fn device_type(&self) -> &'static str {
        self.kind.device_type()
    }

    /// The OAuth `client_id`: lowercase hex of `serial + "#" + device_type`.
    pub fn client_id(&self) -> String {
        hex_lower(format!("{}#{}", self.serial, self.device_type()).as_bytes())
    }

    /// OAuth `assoc_handle`, e.g. `amzn_audible_ios_de` /
    /// `amzn_audible_android_experiment_de`.
    pub fn oauth_assoc_handle(&self, country_code: &str) -> String {
        format!(
            "amzn_audible_{}_{country_code}",
            self.kind.profile().assoc_handle_family
        )
    }

    /// OAuth `pageId`, e.g. `amzn_audible_ios_v2_light_de` /
    /// `amzn_audible_android_aui_v2_dark_de`.
    pub fn oauth_page_id(&self, country_code: &str) -> String {
        format!(
            "amzn_audible_{}_{country_code}",
            self.kind.profile().page_id_family
        )
    }

    /// OAuth `assoc_handle` for the username (pre-merger) login, e.g.
    /// `amzn_audible_ios_lap_de` / `amzn_audible_android_experiment_lap_de`.
    pub fn username_assoc_handle(&self, country_code: &str) -> String {
        format!(
            "amzn_audible_{}_{country_code}",
            self.kind.profile().username_assoc_handle_family
        )
    }

    /// OAuth `pageId` for the username (pre-merger) login, e.g.
    /// `amzn_audible_ios_privatepool` / `amzn_audible_android_privatepool`
    /// (no `<cc>` suffix, matching the reference).
    pub fn username_page_id(&self) -> String {
        format!("amzn_audible_{}", self.kind.profile().username_page_id)
    }

    /// The `User-Agent` for the register request, e.g.
    /// `AmazonWebView/Audible/4.60/iOS/26.2/iPhone` (iOS) or a Dalvik UA
    /// (Android).
    pub fn register_user_agent(&self) -> String {
        let profile = self.kind.profile();
        match self.kind {
            DeviceKind::IPhone => format!(
                "AmazonWebView/{}/{}/iOS/{}/{}",
                profile.app_name, profile.app_version, profile.os_version, profile.device_model
            ),
            DeviceKind::Android => format!(
                "Dalvik/2.1.0 (Linux; U; Android {}; {} Build/RP1A.201005.001)",
                profile.os_version, profile.device_model
            ),
        }
    }

    /// The `registration_data` object for `POST /auth/register`.
    pub fn registration_data(&self) -> serde_json::Value {
        let profile = self.kind.profile();
        serde_json::json!({
            "domain": profile.registration_domain,
            "app_version": profile.app_version,
            "device_type": profile.device_type,
            "device_name": profile.device_name,
            "os_version": profile.os_version,
            "device_serial": self.serial,
            "device_model": profile.device_model,
            "app_name": profile.app_name,
            "software_version": profile.software_version,
        })
    }
}

/// 40 random alphanumeric characters — the app device-serial format. The
/// modulo over the 62-char alphabet has negligible bias for an opaque serial.
fn random_serial() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 40];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

/// Lowercase hex encoding.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_iphone() {
        assert_eq!(DeviceKind::DEFAULT, DeviceKind::IPhone);
        assert_eq!(DeviceKind::DEFAULT.device_type(), "A2CZJZGLK2JJVM");
    }

    #[test]
    fn parses_device_kind() {
        assert_eq!("iphone".parse::<DeviceKind>().unwrap(), DeviceKind::IPhone);
        assert_eq!(
            "Android".parse::<DeviceKind>().unwrap(),
            DeviceKind::Android
        );
        assert!("blackberry".parse::<DeviceKind>().is_err());
    }

    #[test]
    fn serial_is_40_alphanumeric() {
        let device = Device::generate(DeviceKind::IPhone);
        assert_eq!(device.serial().len(), 40);
        assert!(device.serial().chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn client_id_is_hex_of_serial_hash_device_type() {
        let device = Device {
            kind: DeviceKind::IPhone,
            serial: "ABC".to_owned(),
        };
        let expected: String = "ABC#A2CZJZGLK2JJVM"
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(device.client_id(), expected);
        // Ends with the hex of the device type, as the live OAuth URL shows.
        let device_type_hex: String = "A2CZJZGLK2JJVM"
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(device.client_id().ends_with(&device_type_hex));
    }

    #[test]
    fn oauth_handles_per_device() {
        let iphone = Device {
            kind: DeviceKind::IPhone,
            serial: "x".to_owned(),
        };
        assert_eq!(iphone.oauth_assoc_handle("de"), "amzn_audible_ios_de");
        assert_eq!(iphone.oauth_page_id("de"), "amzn_audible_ios_v2_light_de");

        let android = Device {
            kind: DeviceKind::Android,
            serial: "x".to_owned(),
        };
        assert_eq!(
            android.oauth_assoc_handle("de"),
            "amzn_audible_android_experiment_de"
        );
        assert_eq!(
            android.oauth_page_id("de"),
            "amzn_audible_android_aui_v2_dark_de"
        );
    }

    #[test]
    fn username_oauth_handles_per_device() {
        let iphone = Device {
            kind: DeviceKind::IPhone,
            serial: "x".to_owned(),
        };
        assert_eq!(
            iphone.username_assoc_handle("de"),
            "amzn_audible_ios_lap_de"
        );
        assert_eq!(iphone.username_page_id(), "amzn_audible_ios_privatepool");

        let android = Device {
            kind: DeviceKind::Android,
            serial: "x".to_owned(),
        };
        assert_eq!(
            android.username_assoc_handle("de"),
            "amzn_audible_android_experiment_lap_de"
        );
        assert_eq!(
            android.username_page_id(),
            "amzn_audible_android_privatepool"
        );
    }

    #[test]
    fn registration_data_has_the_profile_fields() {
        let device = Device {
            kind: DeviceKind::IPhone,
            serial: "SERIAL123".to_owned(),
        };
        let data = device.registration_data();
        assert_eq!(data["device_type"], "A2CZJZGLK2JJVM");
        assert_eq!(data["device_serial"], "SERIAL123");
        assert_eq!(data["domain"], "Device");
        assert_eq!(data["software_version"], "46000856");
    }
}
