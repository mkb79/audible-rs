//! Sign-in challenge detection and the UI-free prompt interface.
//!
//! Detection mirrors the reference `login/challenges/*`: which page element or
//! form field identifies each challenge. The [`ChallengePrompt`] trait keeps the
//! core UI-free — the CLI provides a terminal implementation; the login flow
//! never does I/O itself.

use super::LoginError;
use super::page::Page;

/// An MFA device the user can pick to receive a one-time code.
pub struct MfaDevice {
    /// The radio `value` submitted as `otpDeviceContext`.
    pub value: String,
    /// Human label (e.g. a masked phone number or "Authenticator app").
    pub label: String,
    /// Method type from the `auth-<METHOD>` class (`TOTP`, `SMS`, `VOICE`, …).
    pub method: String,
}

/// A detected sign-in challenge.
pub(crate) enum Challenge {
    /// Image CAPTCHA — solve the distorted text.
    Captcha { image_url: String },
    /// Choose which registered device receives the one-time code.
    MfaChoice(Vec<MfaDevice>),
    /// One-time password (2FA / authenticator).
    Otp,
    /// Challenge verification (CVF) — a code sent to email/SMS.
    Cvf,
    /// Amazon's JavaScript-driven anti-automation ("aamation") verification
    /// page (AUD-178). It reuses the `cvf-page-content` anchor but leaves it
    /// empty and renders the actual challenge from JavaScript, so a scripted
    /// client sees only "This site requires JavaScript". **No verification
    /// code is ever sent** — prompting for one strands the user (the reported
    /// "never receive a code"). Unanswerable without a browser.
    AntiAutomation,
    /// Approval alert — approve the push/email notification on another device.
    Approval,
}

impl Challenge {
    /// Detects the challenge on `page`, in the reference's priority order.
    /// `None` when the page carries no known challenge (success or an error).
    pub fn detect(page: &Page) -> Option<Challenge> {
        if let Some(image_url) = page.captcha_image_url() {
            return Some(Challenge::Captcha { image_url });
        }
        if page.has_input("otpDeviceContext") {
            let devices = page
                .mfa_devices()
                .into_iter()
                .map(|(value, label, method)| MfaDevice {
                    value,
                    label,
                    method,
                })
                .collect();
            return Some(Challenge::MfaChoice(devices));
        }
        if page.has_input("otpCode") {
            return Some(Challenge::Otp);
        }
        // Anti-automation before CVF: the page carries the same
        // `cvf-page-content` anchor, so its own marker fields must win —
        // otherwise it is misread as a code challenge (AUD-178).
        if page.has_input("cvf_aamation_response_token")
            || page.has_input("cvf_aamation_error_code")
        {
            return Some(Challenge::AntiAutomation);
        }
        if page.has_id("cvf-page-content") {
            return Some(Challenge::Cvf);
        }
        if page.has_id("resend-approval-alert") || page.has_id("resend-approval-form") {
            return Some(Challenge::Approval);
        }
        None
    }
}

/// UI-free interface the scripted login uses to obtain challenge answers. The
/// CLI implements it with terminal prompts (and inline captcha rendering); the
/// auth core never reads stdin or prints itself.
pub trait ChallengePrompt: Send + Sync {
    /// Show an informational notice from the sign-in page before the next
    /// prompt — e.g. a warning ("we can't send an SMS to …") or a retry error
    /// ("that code was incorrect").
    fn notice(&self, message: &str);
    /// Solve a CAPTCHA, given its URL and the downloaded image bytes.
    fn captcha(&self, image_url: &str, image: &[u8]) -> Result<String, LoginError>;
    /// The OTP / 2FA code.
    fn otp(&self) -> Result<String, LoginError>;
    /// The CVF code (sent to the user's email/phone).
    fn cvf(&self) -> Result<String, LoginError>;
    /// Pick one of the offered MFA devices; returns the chosen index.
    fn mfa_choice(&self, devices: &[MfaDevice]) -> Result<usize, LoginError>;
    /// Tell the user to approve the push/email notification, then wait for them
    /// to confirm they have done so.
    fn approval(&self) -> Result<(), LoginError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(html: &str) -> Option<Challenge> {
        Challenge::detect(&Page::parse(html))
    }

    #[test]
    fn detects_each_challenge() {
        assert!(matches!(
            detect(r#"<img id="auth-captcha-image" src="https://x/captcha.jpg"/>"#),
            Some(Challenge::Captcha { .. })
        ));
        assert!(matches!(
            detect(r#"<form id="auth-mfa-form"><input name="otpCode"/></form>"#),
            Some(Challenge::Otp)
        ));
        assert!(matches!(
            detect(r#"<div id="cvf-page-content"></div>"#),
            Some(Challenge::Cvf)
        ));
        assert!(matches!(
            detect(r#"<div id="resend-approval-alert"></div>"#),
            Some(Challenge::Approval)
        ));
        assert!(detect(r#"<form name="signIn"><input name="email"/></form>"#).is_none());
    }

    /// The anti-automation page (AUD-178) reuses the `cvf-page-content`
    /// anchor but leaves it empty and ships `cvf_aamation_*` fields with no
    /// `code` input — it must never be read as a CVF code challenge. Shape
    /// taken from a real `.ca` sign-in (2026-07-14).
    #[test]
    fn anti_automation_page_is_not_a_cvf_code_challenge() {
        let html = r#"
            <div id="cvf-page-content">
              <noscript><h4>JavaScript Is Disabled</h4></noscript>
            </div>
            <form method="post" action="verify">
              <input type="hidden" name="cvf_aamation_response_token" value="x"/>
              <input type="hidden" name="cvf_captcha_captcha_action" value="y"/>
              <input type="hidden" name="cvf_aamation_error_code" value=""/>
              <input type="hidden" name="verifyToken" value="z"/>
            </form>"#;
        assert!(matches!(detect(html), Some(Challenge::AntiAutomation)));

        // A genuine CVF page (the anchor, no aamation markers) still resolves
        // to the code challenge.
        assert!(matches!(
            detect(r#"<div id="cvf-page-content"><input name="code"/></div>"#),
            Some(Challenge::Cvf)
        ));
    }

    /// The 2FA paths must never be shadowed by the anti-automation check:
    /// OTP entry and MFA device choice are detected first, so an account with
    /// 2FA enabled keeps working even if Amazon were to ship aamation markers
    /// on the same page (AUD-178).
    #[test]
    fn otp_and_mfa_win_over_anti_automation() {
        let otp = r#"
            <form id="auth-mfa-form">
              <input name="otpCode"/>
              <input type="hidden" name="cvf_aamation_response_token" value="x"/>
            </form>"#;
        assert!(matches!(detect(otp), Some(Challenge::Otp)));

        let choice = r#"
            <form id="auth-select-device-form">
              <div data-a-input-name="otpDeviceContext" class="a-radio auth-TOTP">
                <input type="radio" name="otpDeviceContext" value="ctx1, TOTP"/>
                <span class="a-label">Authenticator app</span>
              </div>
              <input type="hidden" name="cvf_aamation_error_code" value=""/>
            </form>"#;
        assert!(matches!(detect(choice), Some(Challenge::MfaChoice(_))));
    }

    #[test]
    fn detects_mfa_choice_with_devices() {
        let html = r#"
            <form id="auth-select-device-form">
              <div data-a-input-name="otpDeviceContext" class="a-radio auth-TOTP">
                <input type="radio" name="otpDeviceContext" value="ctx1, TOTP"/>
                <span class="a-label">Authenticator app</span>
              </div>
              <div data-a-input-name="otpDeviceContext" class="a-radio auth-SMS">
                <input type="radio" name="otpDeviceContext" value="ctx2, SMS"/>
                <span class="a-label">SMS to ***-1234</span>
              </div>
            </form>"#;
        let Some(Challenge::MfaChoice(devices)) = detect(html) else {
            panic!("expected MfaChoice");
        };
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].method, "TOTP");
        assert_eq!(devices[0].label, "Authenticator app");
        assert_eq!(devices[1].value, "ctx2, SMS");
        assert_eq!(devices[1].method, "SMS");
    }
}
