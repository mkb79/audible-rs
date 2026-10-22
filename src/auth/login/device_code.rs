//! Device-code sign-in — the default `account login` (mkb79/Audible#906).
//!
//! Amazon's code-based device linking, the way a TV or smart speaker signs
//! in: the device asks `POST /auth/create/codepair` for a short code, the
//! user signs in at `amazon.<tld>/code` in any browser and enters it, and
//! the device polls `POST /auth/register` with the pair until Amazon has
//! linked it — the same registration the browser flows do, with the code
//! pair as `auth_data` instead of an OAuth code. The password never passes
//! through this program, and 2FA/CAPTCHA stay on Amazon's own pages.
//!
//! UI-free: nothing here prints — the caller shows [`CodePair::public_code`]
//! and [`verification_url`] and decides how to wait. The request shapes
//! follow the reference's test plugin for the issue; the device identity is
//! the [`Device`] profile every other flow registers with. No secrets are
//! logged (the private code is a [`SecretString`]).

use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::api::locale::Locale;
use crate::auth::Authenticator;
use crate::auth::device::Device;

use super::{IdentityApi, LoginError, build_authenticator, register_request, registration_body};

/// The MAP domain a code pair is created in. Always `Device`, whatever the
/// profile registers under: `/auth/create/codepair` rejects the Android
/// profile's `DeviceLegacy` with `HTTP 400 InvalidValue` (verified live),
/// while `registration_data.domain` keeps the profile's value.
const CODE_PAIR_DOMAIN: &str = "Device";
/// Validity asked of Amazon for a code, in seconds (the response's
/// `expires_in` is what counts).
const CODE_DURATION_SECS: u32 = 600;
/// Length of the code the user types.
const CODE_LENGTH: u32 = 5;
/// Polls never come faster than this, whatever interval Amazon returns.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Cap on the backoff after a throttled or failed poll.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The backoff doubles at most this many times.
const MAX_BACKOFF_DOUBLINGS: u32 = 4;

/// A code pair from `POST /auth/create/codepair`: the public code the user
/// enters at the verification page, the private code only this device
/// knows, and when / how often Amazon wants to be asked about it.
pub struct CodePair {
    public_code: String,
    private_code: SecretString,
    expires_at: Instant,
    interval: Duration,
}

impl CodePair {
    /// The code the user enters at [`verification_url`].
    pub fn public_code(&self) -> &str {
        &self.public_code
    }

    /// How long Amazon still accepts the code (zero once it has expired).
    pub fn time_left(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// The pause between two polls, as Amazon asked for it (at least one
    /// second).
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Parses the code-pair response. `requested_at` is when the request
    /// went out: Amazon counts the validity from when it issued the code, so
    /// the local deadline starts no later than that.
    fn from_response(payload: &Value, requested_at: Instant) -> Result<Self, LoginError> {
        let public_code = non_empty_str(payload, "public_code")?.to_owned();
        let private_code = non_empty_str(payload, "private_code")?.to_owned();
        let expires_in = positive_secs(payload, "expires_in")?;
        let interval = positive_secs(payload, "polling_interval_in_seconds")?;
        let expires_at = requested_at
            .checked_add(expires_in)
            .ok_or(LoginError::CodePairResponse)?;
        Ok(Self {
            public_code,
            private_code: SecretString::from(private_code),
            expires_at,
            interval: interval.max(MIN_INTERVAL),
        })
    }
}

/// The page where the user enters the code: `https://www.amazon.<tld>/code`.
pub fn verification_url(locale: &Locale) -> String {
    format!("https://www.amazon.{}/code", locale.domain)
}

/// Asks Amazon for a code pair for `device` (`POST /auth/create/codepair` on
/// the marketplace's identity API). The caller shows the code and the URL.
pub async fn create_code_pair(
    http: &Client,
    locale: &Locale,
    device: &Device,
) -> Result<CodePair, LoginError> {
    create_code_pair_via(http, &IdentityApi::new(locale, false), device).await
}

/// [`create_code_pair`] against an explicit API (tests point it at a mock).
pub(crate) async fn create_code_pair_via(
    http: &Client,
    api: &IdentityApi,
    device: &Device,
) -> Result<CodePair, LoginError> {
    // The pair is bound to the device the registration will carry: the same
    // serial and type as `registration_data` (the domain is always `Device`).
    tracing::debug!(
        device_type = device.device_type(),
        domain = CODE_PAIR_DOMAIN,
        "requesting a device code"
    );
    let body = serde_json::json!({
        "code_data": {
            "device_serial": device.serial(),
            "device_type": device.device_type(),
            "domain": CODE_PAIR_DOMAIN,
        },
        "code_properties": {
            "code_duration": CODE_DURATION_SECS.to_string(),
            "code_length": CODE_LENGTH.to_string(),
        },
    });
    let requested_at = Instant::now();
    let response = http
        .post(api.url("/auth/create/codepair"))
        .header("x-amzn-identity-auth-domain", api.auth_domain())
        .header(reqwest::header::USER_AGENT, device.register_user_agent())
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let payload = json_object(response).await;
    // An error can also ride in a 200 body.
    let error = AmazonError::from_payload(&payload);
    if !status.is_success() || error.code.is_some() {
        return Err(LoginError::CodePair(error.describe(status)));
    }
    CodePair::from_response(&payload, requested_at)
}

/// Registers `device` once the user has confirmed `code_pair`: polls `POST
/// /auth/register` with the pair — "not yet" is an HTTP 401 `Unauthorized`
/// — until Amazon answers the registration, the code expires
/// ([`LoginError::CodeExpired`]) or Amazon rejects it
/// ([`LoginError::CodeRejected`]). Throttling (429), server errors and
/// network failures back off and retry within the code's lifetime; give
/// `http` a request timeout so a stalled poll counts as one of those.
pub async fn register_with_code_pair(
    http: &Client,
    locale: &Locale,
    device: &Device,
    code_pair: &CodePair,
) -> Result<Authenticator, LoginError> {
    register_with_code_pair_via(
        http,
        &IdentityApi::new(locale, false),
        locale,
        device,
        code_pair,
    )
    .await
}

/// [`register_with_code_pair`] against an explicit API (tests point it at a
/// mock).
pub(crate) async fn register_with_code_pair_via(
    http: &Client,
    api: &IdentityApi,
    locale: &Locale,
    device: &Device,
    code_pair: &CodePair,
) -> Result<Authenticator, LoginError> {
    let auth_data = serde_json::json!({
        "code_pair": {
            "public_code": code_pair.public_code,
            "private_code": code_pair.private_code.expose_secret(),
        },
        "use_global_authentication": "true",
    });
    // One body for the whole poll, so the `frc` device context stays the same.
    let body = registration_body(locale, device, auth_data);

    let mut delay = code_pair.interval;
    let mut failures = 0u32;
    loop {
        tokio::time::sleep(delay.min(code_pair.time_left())).await;
        if code_pair.time_left().is_zero() {
            return Err(LoginError::CodeExpired);
        }

        let response = match register_request(http, api, locale, device, &body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                failures += 1;
                delay = retry_delay(None, code_pair.interval, failures);
                tracing::warn!(%error, retry_in = ?delay, "device-code poll failed, retrying");
                continue;
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            failures += 1;
            delay = retry_delay(retry_after(&response), code_pair.interval, failures);
            tracing::warn!(
                %status,
                retry_in = ?delay,
                "device-code poll throttled or failed upstream, retrying"
            );
            continue;
        }
        let payload = json_object(response).await;
        if status.is_success()
            && let Some(success) = payload.pointer("/response/success")
        {
            let auth = build_authenticator(locale, success).await?;
            // The pair was created for this serial; a registration for
            // another device would be a mix-up, not this login.
            if auth
                .device_serial()
                .is_some_and(|serial| serial != device.serial())
            {
                return Err(LoginError::ForeignDevice);
            }
            return Ok(auth);
        }
        let error = AmazonError::from_payload(&payload);
        if status == StatusCode::UNAUTHORIZED && error.is_pending() {
            failures = 0;
            delay = code_pair.interval;
            continue;
        }
        return Err(LoginError::CodeRejected(error.describe(status)));
    }
}

/// The JSON object a response carries, or an empty object when the body is
/// not one (Amazon's error pages are not always JSON).
async fn json_object(response: Response) -> Value {
    match response.json::<Value>().await {
        Ok(value) if value.is_object() => value,
        _ => Value::Object(Default::default()),
    }
}

/// The error Amazon's identity API sent, if any, in either shape it uses:
/// the MAP envelope `{"response": {"error": {"code": …, "message": …}}}`
/// (or that object at the top level) and the OAuth form `{"error": "…",
/// "error_description": "…"}`.
struct AmazonError {
    code: Option<String>,
    message: Option<String>,
}

impl AmazonError {
    fn from_payload(payload: &Value) -> Self {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
        let error = payload
            .get("error")
            .or_else(|| payload.pointer("/response/error"));
        match error {
            Some(Value::String(code)) => Self {
                code: Some(code.clone()),
                message: text(payload.get("error_description")),
            },
            Some(Value::Object(fields)) => Self {
                code: ["code", "error_code", "error", "type"]
                    .into_iter()
                    .find_map(|key| text(fields.get(key))),
                message: text(fields.get("message")),
            },
            _ => Self {
                code: None,
                message: None,
            },
        }
    }

    /// "Not confirmed yet": the `Unauthorized` a poll expects until the user
    /// has entered the code.
    fn is_pending(&self) -> bool {
        self.code
            .as_deref()
            .is_some_and(|code| code.eq_ignore_ascii_case("Unauthorized"))
    }

    /// `HTTP 400 InvalidValue: One or more provided values are invalid`.
    fn describe(&self, status: StatusCode) -> String {
        let mut text = format!("HTTP {}", status.as_u16());
        if let Some(code) = &self.code {
            text.push(' ');
            text.push_str(code);
        }
        if let Some(message) = &self.message {
            text.push_str(": ");
            text.push_str(message);
        }
        text
    }
}

/// A numeric `Retry-After` header, in seconds (the HTTP-date form is ignored).
fn retry_after(response: &Response) -> Option<Duration> {
    let secs: f64 = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

/// The pause before the next poll after `failures` consecutive failures: the
/// interval doubled per failure (at most [`MAX_BACKOFF_DOUBLINGS`] times,
/// capped at [`MAX_BACKOFF`]), raised to `retry_after` when Amazon asked for
/// a longer pause, and never shorter than the interval.
fn retry_delay(retry_after: Option<Duration>, interval: Duration, failures: u32) -> Duration {
    let backoff = interval
        .saturating_mul(1 << failures.min(MAX_BACKOFF_DOUBLINGS))
        .min(MAX_BACKOFF);
    let delay = match retry_after {
        Some(wanted) => backoff.max(wanted),
        None => backoff,
    };
    delay.max(interval)
}

fn non_empty_str<'a>(payload: &'a Value, key: &str) -> Result<&'a str, LoginError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::CodePairResponse)
}

/// A positive number of seconds — Amazon sends numbers as numbers or strings.
fn positive_secs(payload: &Value, key: &str) -> Result<Duration, LoginError> {
    let secs = match payload.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|secs| secs.is_finite() && *secs > 0.0)
    .ok_or(LoginError::CodePairResponse)?;
    Duration::try_from_secs_f64(secs).map_err(|_| LoginError::CodePairResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::locale;
    use crate::auth::device::DeviceKind;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn de() -> Locale {
        locale::find("de").unwrap()
    }

    fn api(server: &MockServer) -> IdentityApi {
        IdentityApi::mocked(&server.uri(), &de())
    }

    fn pair(interval: Duration, valid_for: Duration) -> CodePair {
        CodePair {
            public_code: "ABCDE".to_owned(),
            private_code: SecretString::from("private-code".to_owned()),
            expires_at: Instant::now() + valid_for,
            interval,
        }
    }

    /// Amazon's "not confirmed yet" answer.
    fn pending() -> ResponseTemplate {
        ResponseTemplate::new(401).set_body_json(json!({
            "response": { "error": {
                "code": "Unauthorized",
                "message": "the code has not been confirmed yet",
            } },
        }))
    }

    /// A register `success` payload for `serial`, with the synthetic fixture
    /// key (throwaway, never real credentials).
    fn register_success(serial: &str) -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/signing.json");
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string(path)
                .expect("missing tests/fixtures/signing.json — run scripts/gen_fixtures.py once"),
        )
        .unwrap();
        json!({
            "response": { "success": {
                "extensions": {
                    "device_info": {
                        "device_name": "Alice's Audible for iPhone",
                        "device_serial_number": serial,
                        "device_type": "A2CZJZGLK2JJVM",
                    },
                    "customer_info": {
                        "account_pool": "Amazon",
                        "user_id": "amzn1.account.EXAMPLE",
                        "home_region": "EU",
                        "name": "Alice",
                        "given_name": "Alice",
                    },
                },
                "tokens": {
                    "website_cookies": [],
                    "mac_dms": {
                        "device_private_key": fixture["device_private_key"],
                        "adp_token": fixture["adp_token"],
                    },
                    "bearer": {
                        "access_token": "Atna|EXAMPLE",
                        "refresh_token": "Atnr|EXAMPLE",
                        "expires_in": "3600",
                    },
                },
                "customer_id": "amzn1.account.EXAMPLE",
            } },
            "request_id": "req-1",
        })
    }

    #[test]
    fn verification_url_is_the_amazon_code_page() {
        assert_eq!(verification_url(&de()), "https://www.amazon.de/code");
        assert_eq!(
            verification_url(&locale::find("uk").unwrap()),
            "https://www.amazon.co.uk/code"
        );
    }

    #[tokio::test]
    async fn creates_a_code_pair_from_the_response() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .and(header("x-amzn-identity-auth-domain", "api.amazon.de"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(json!({
                "code_data": {
                    "device_serial": device.serial(),
                    "device_type": "A2CZJZGLK2JJVM",
                    "domain": "Device",
                },
                "code_properties": { "code_duration": "600", "code_length": "5" },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_code": "ABCDE",
                "private_code": "private-code",
                "expires_in": 600,
                "polling_interval_in_seconds": "0.2",
                "verification_uri": "https://www.amazon.de/code",
                "user_code": "ABCDE",
                "device_code": "not-read",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pair = create_code_pair_via(&Client::new(), &api(&server), &device)
            .await
            .unwrap();
        assert_eq!(pair.public_code(), "ABCDE");
        assert_eq!(pair.private_code.expose_secret(), "private-code");
        // Amazon's interval is clamped to the floor.
        assert_eq!(pair.interval(), MIN_INTERVAL);
        assert!(pair.time_left() > Duration::from_secs(590));
        assert!(pair.time_left() <= Duration::from_secs(600));
    }

    /// The Android profile registers under `DeviceLegacy`, but its code pair
    /// is still created in the `Device` domain (anything else is rejected).
    #[tokio::test]
    async fn android_code_pair_is_created_in_the_device_domain() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::Android);
        assert_eq!(device.registration_data()["domain"], "DeviceLegacy");
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .and(body_partial_json(json!({
                "code_data": {
                    "device_serial": device.serial(),
                    "device_type": "A10KISP2GWF0E4",
                    "domain": "Device",
                },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_code": "ABCDE",
                "private_code": "private-code",
                "expires_in": 600,
                "polling_interval_in_seconds": 5,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pair = create_code_pair_via(&Client::new(), &api(&server), &device)
            .await
            .unwrap();
        assert_eq!(pair.public_code(), "ABCDE");
    }

    #[tokio::test]
    async fn code_pair_errors_are_reported() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "response": { "error": { "code": "InvalidValue", "message": "nope" } },
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Then a 200 without a private code, then one with a zero validity.
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_code": "ABCDE",
                "expires_in": 600,
                "polling_interval_in_seconds": 5,
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_code": "ABCDE",
                "private_code": "private-code",
                "expires_in": 0,
                "polling_interval_in_seconds": 5,
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // An error riding in a 200 body is still an error.
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "response": { "error": { "code": "ServerError", "message": "try later" } },
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // A non-JSON error page.
        Mock::given(method("POST"))
            .and(path("/auth/create/codepair"))
            .respond_with(ResponseTemplate::new(503).set_body_string("<html>down</html>"))
            .mount(&server)
            .await;

        let http = Client::new();
        assert!(matches!(
            create_code_pair_via(&http, &api(&server), &device).await,
            Err(LoginError::CodePair(detail)) if detail == "HTTP 400 InvalidValue: nope"
        ));
        assert!(matches!(
            create_code_pair_via(&http, &api(&server), &device).await,
            Err(LoginError::CodePairResponse)
        ));
        assert!(matches!(
            create_code_pair_via(&http, &api(&server), &device).await,
            Err(LoginError::CodePairResponse)
        ));
        assert!(matches!(
            create_code_pair_via(&http, &api(&server), &device).await,
            Err(LoginError::CodePair(detail)) if detail == "HTTP 200 ServerError: try later"
        ));
        assert!(matches!(
            create_code_pair_via(&http, &api(&server), &device).await,
            Err(LoginError::CodePair(detail)) if detail == "HTTP 503"
        ));
    }

    #[tokio::test]
    async fn polls_until_the_code_is_confirmed() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        // Two "not yet" answers, then the registration.
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(pending())
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .and(header("x-amzn-identity-auth-domain", "api.amazon.de"))
            .and(header("accept", "application/json"))
            .and(body_partial_json(json!({
                "auth_data": {
                    "code_pair": { "public_code": "ABCDE", "private_code": "private-code" },
                    "use_global_authentication": "true",
                },
                "registration_data": {
                    "device_serial": device.serial(),
                    "device_type": "A2CZJZGLK2JJVM",
                },
                "requested_token_type": ["bearer", "mac_dms", "website_cookies"],
                "requested_extensions": ["device_info", "customer_info"],
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(register_success(device.serial())),
            )
            .expect(1)
            .mount(&server)
            .await;

        let pair = pair(Duration::from_millis(10), Duration::from_secs(30));
        let auth =
            register_with_code_pair_via(&Client::new(), &api(&server), &de(), &device, &pair)
                .await
                .unwrap();
        assert_eq!(auth.device_serial(), Some(device.serial()));
        assert_eq!(auth.device_name(), Some("Alice's Audible for iPhone"));
        assert_eq!(auth.customer_id(), Some("amzn1.account.EXAMPLE"));
        assert_eq!(auth.locale().country_code, "de");
        assert!(auth.signer().is_some());
        assert!(!auth.access_token_expired());
    }

    #[tokio::test]
    async fn a_rejected_code_stops_the_poll() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        // A 401 that is not the pending `Unauthorized`, then a 400.
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "response": { "error": { "code": "InvalidToken", "message": "expired" } },
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "response": { "error": {
                    "code": "InvalidValue",
                    "message": "One or more provided values are invalid",
                } },
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // The OAuth error form, then a 200 without a registration in it.
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "the code pair is unknown",
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "response": { "challenge": {} },
            })))
            .mount(&server)
            .await;

        let http = Client::new();
        let pair = pair(Duration::from_millis(10), Duration::from_secs(30));
        for expected in [
            "HTTP 401 InvalidToken: expired",
            "HTTP 400 InvalidValue: One or more provided values are invalid",
            "HTTP 400 invalid_grant: the code pair is unknown",
            "HTTP 200",
        ] {
            let error = register_with_code_pair_via(&http, &api(&server), &de(), &device, &pair)
                .await
                .unwrap_err();
            assert!(matches!(error, LoginError::CodeRejected(_)));
            assert_eq!(
                error.to_string(),
                format!("Amazon rejected the device code ({expected})")
            );
        }
    }

    #[test]
    fn amazon_errors_are_read_in_every_shape() {
        let describe =
            |payload: Value| AmazonError::from_payload(&payload).describe(StatusCode::BAD_REQUEST);
        assert_eq!(
            describe(
                json!({ "response": { "error": { "code": "InvalidValue", "message": "bad" } } })
            ),
            "HTTP 400 InvalidValue: bad"
        );
        assert_eq!(
            describe(json!({ "error": { "error_code": "Throttled" } })),
            "HTTP 400 Throttled"
        );
        assert_eq!(
            describe(json!({ "error": { "type": "ServerError", "message": "later" } })),
            "HTTP 400 ServerError: later"
        );
        assert_eq!(
            describe(json!({ "error": "invalid_request", "error_description": "no" })),
            "HTTP 400 invalid_request: no"
        );
        assert_eq!(describe(json!({ "error": null })), "HTTP 400");
        assert_eq!(describe(json!({})), "HTTP 400");
        assert!(
            AmazonError::from_payload(
                &json!({ "response": { "error": { "code": "UNAUTHORIZED" } } })
            )
            .is_pending()
        );
        assert!(
            !AmazonError::from_payload(&json!({ "error": "unauthorized_client" })).is_pending()
        );
    }

    #[tokio::test]
    async fn an_expired_code_ends_the_poll() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(pending())
            .mount(&server)
            .await;

        let http = Client::new();
        // Already expired: no request goes out.
        let pair = pair(Duration::from_millis(10), Duration::ZERO);
        assert!(matches!(
            register_with_code_pair_via(&http, &api(&server), &de(), &device, &pair).await,
            Err(LoginError::CodeExpired)
        ));
        assert!(server.received_requests().await.unwrap().is_empty());

        // Never confirmed: polled until the deadline, then given up.
        let pair = super::tests::pair(Duration::from_millis(10), Duration::from_millis(80));
        assert!(matches!(
            register_with_code_pair_via(&http, &api(&server), &de(), &device, &pair).await,
            Err(LoginError::CodeExpired)
        ));
        assert!(!server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn backs_off_on_upstream_trouble_then_succeeds() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(register_success(device.serial())),
            )
            .expect(1)
            .mount(&server)
            .await;

        let pair = pair(Duration::from_millis(10), Duration::from_secs(30));
        let auth =
            register_with_code_pair_via(&Client::new(), &api(&server), &de(), &device, &pair)
                .await
                .unwrap();
        assert_eq!(auth.device_serial(), Some(device.serial()));
    }

    #[tokio::test]
    async fn a_registration_for_another_device_is_refused() {
        let server = MockServer::start().await;
        let device = Device::generate(DeviceKind::IPhone);
        Mock::given(method("POST"))
            .and(path("/auth/register"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(register_success("SOMEOTHERSERIAL")),
            )
            .mount(&server)
            .await;

        let pair = pair(Duration::from_millis(10), Duration::from_secs(30));
        assert!(matches!(
            register_with_code_pair_via(&Client::new(), &api(&server), &de(), &device, &pair).await,
            Err(LoginError::ForeignDevice)
        ));
    }

    #[test]
    fn retry_delay_doubles_caps_and_honours_retry_after() {
        let interval = Duration::from_secs(5);
        assert_eq!(retry_delay(None, interval, 1), Duration::from_secs(10));
        assert_eq!(retry_delay(None, interval, 2), Duration::from_secs(20));
        assert_eq!(retry_delay(None, interval, 3), Duration::from_secs(40));
        // Capped at the maximum, however many failures.
        assert_eq!(retry_delay(None, interval, 4), MAX_BACKOFF);
        assert_eq!(retry_delay(None, interval, 40), MAX_BACKOFF);
        // Amazon's Retry-After only ever lengthens the pause.
        assert_eq!(
            retry_delay(Some(Duration::from_secs(120)), interval, 1),
            Duration::from_secs(120)
        );
        assert_eq!(
            retry_delay(Some(Duration::from_secs(1)), interval, 1),
            Duration::from_secs(10)
        );
        // Never shorter than the interval itself.
        let long = Duration::from_secs(70);
        assert_eq!(retry_delay(None, long, 1), long);
    }

    #[test]
    fn code_pair_response_accepts_numbers_as_strings() {
        let now = Instant::now();
        let pair = CodePair::from_response(
            &json!({
                "public_code": " ABCDE ",
                "private_code": "private-code",
                "expires_in": "600",
                "polling_interval_in_seconds": 5,
            }),
            now,
        )
        .unwrap();
        assert_eq!(pair.public_code(), "ABCDE");
        assert_eq!(pair.interval(), Duration::from_secs(5));
        assert_eq!(pair.expires_at, now + Duration::from_secs(600));

        for broken in [
            json!({ "private_code": "p", "expires_in": 600, "polling_interval_in_seconds": 5 }),
            json!({ "public_code": "", "private_code": "p", "expires_in": 600, "polling_interval_in_seconds": 5 }),
            json!({ "public_code": "A", "private_code": "p", "expires_in": "soon", "polling_interval_in_seconds": 5 }),
            json!({ "public_code": "A", "private_code": "p", "expires_in": 600, "polling_interval_in_seconds": -1 }),
        ] {
            assert!(matches!(
                CodePair::from_response(&broken, now),
                Err(LoginError::CodePairResponse)
            ));
        }
    }
}
