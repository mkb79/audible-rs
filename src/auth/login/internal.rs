//! The scripted ("internal") login flow (AUD-59): a form-driven Amazon sign-in
//! with challenge handling, then device registration — no external browser.
//!
//! The HTTP client carries a cookie store so the sign-in session survives
//! across requests. No `!Send` HTML is held across an `.await`: each page is
//! parsed, the needed data taken as owned values, and the [`Page`] dropped
//! before the next request. No credentials are logged.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Client, Url};

use crate::api::locale::Locale;
use crate::auth::Authenticator;
use crate::auth::device::Device;

use super::challenge::{Challenge, ChallengePrompt};
use super::page::{Form, Page};
use super::{LoginError, Pkce, authorize_url, metadata1, register};

/// A mobile-Safari User-Agent for the sign-in HTML flow (the webview the iOS
/// app uses). Also fed into `metadata1.location`/`userAgent`.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 15_0 like Mac OS X) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";

/// Hard cap on the challenge loop.
const MAX_STEPS: usize = 16;
/// Approval-alert polling cadence and budget (~2 minutes).
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_secs(2);
const APPROVAL_POLL_MAX: usize = 60;

/// Runs the scripted login and returns the registered [`Authenticator`].
/// `prompt` answers any challenges (the CLI provides a terminal impl).
#[allow(clippy::too_many_arguments)]
pub async fn login_internal(
    locale: &Locale,
    device: &Device,
    pkce: &Pkce,
    email: &str,
    password: &str,
    prompt: &dyn ChallengePrompt,
    with_username: bool,
) -> Result<Authenticator, LoginError> {
    let http = Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .cookie_store(true)
        .user_agent(BROWSER_USER_AGENT)
        .redirect(maplanding_redirect_policy())
        .build()?;

    let authorize = authorize_url(device, pkce, locale, with_username);
    let metadata =
        metadata1::encrypt_metadata(&metadata1::meta_audible_app(BROWSER_USER_AGENT, &authorize));

    // Fetch the sign-in page, then submit the credentials.
    let resp = http.get(&authorize).send().await?;
    let mut url = resp.url().clone();
    let mut text = resp.text().await?;
    dump_page(0, "signin", &text);
    (url, text) = submit_credentials(&http, &url, &text, email, password, &metadata).await?;

    // Resolve challenges until the redirect carries the authorization code.
    for step in 0..MAX_STEPS {
        if let Some(code) = auth_code(&url) {
            return register(&http, locale, device, pkce, &code, with_username).await;
        }
        dump_page(step + 1, "challenge", &text);

        // Parse the page and take owned data, dropping the !Send page here.
        #[allow(clippy::type_complexity)]
        let (form, challenge, error, warning): (
            Option<Form>,
            Option<Challenge>,
            Option<String>,
            Option<String>,
        ) = {
            let page = Page::parse(&text);
            (
                page.form(None),
                Challenge::detect(&page),
                page.error_message(),
                page.warning_message(),
            )
        };
        // Surface page notices so the user understands the next step: a
        // warning always, and a page error only when a challenge follows (a
        // retry hint like "incorrect code"; a terminal error is returned below).
        if let Some(message) = warning.as_deref() {
            prompt.notice(message);
        }
        if challenge.is_some()
            && let Some(message) = error.as_deref()
        {
            prompt.notice(message);
        }

        // Anti-automation is terminal: there is nothing to answer (no code is
        // sent) and no form we could complete without a JavaScript engine.
        // Fail with the browser-flow hint instead of prompting for a code
        // that never arrives (AUD-178).
        if matches!(challenge, Some(Challenge::AntiAutomation)) {
            return Err(LoginError::AntiAutomation);
        }
        // Approval needs no form (poll a GET until approved).
        if matches!(challenge, Some(Challenge::Approval)) {
            prompt.approval()?;
            (url, text) = poll_approval(&http, &url).await?;
            continue;
        }
        let Some(challenge) = challenge else {
            return Err(LoginError::LoginFailed(
                error.unwrap_or_else(|| "login could not be completed".to_owned()),
            ));
        };
        let Some(form) = form else {
            return Err(LoginError::LoginFailed(error.unwrap_or_else(|| {
                "expected a form on the challenge page".to_owned()
            })));
        };
        let mut inputs = form.inputs;

        match challenge {
            Challenge::Captcha { image_url } => {
                let image = http.get(&image_url).send().await?.bytes().await?;
                let guess = prompt.captcha(&image_url, &image)?;
                inputs.insert("email".to_owned(), email.to_owned());
                inputs.insert("password".to_owned(), password.to_owned());
                inputs.insert("guess".to_owned(), guess);
            }
            Challenge::MfaChoice(devices) => {
                if devices.is_empty() {
                    return Err(LoginError::LoginFailed("no MFA devices offered".to_owned()));
                }
                let index = prompt.mfa_choice(&devices)?.min(devices.len() - 1);
                inputs.insert("otpDeviceContext".to_owned(), devices[index].value.clone());
            }
            Challenge::Otp => {
                inputs.insert("otpCode".to_owned(), prompt.otp()?);
                inputs.insert("rememberDevice".to_owned(), "false".to_owned());
            }
            Challenge::Cvf => {
                inputs.insert("code".to_owned(), prompt.cvf()?);
            }
            Challenge::AntiAutomation | Challenge::Approval => unreachable!("handled above"),
        }
        (url, text) = submit(&http, &url, &form.action, &inputs).await?;
    }
    Err(LoginError::TooManyChallenges)
}

/// Submits the sign-in form, handling Amazon's separated email/password pages
/// (`appAction=SIGNIN_PWD_COLLECT` / `subPageType=SignInClaimCollect`).
async fn submit_credentials(
    http: &Client,
    base: &Url,
    text: &str,
    email: &str,
    password: &str,
    metadata: &str,
) -> Result<(Url, String), LoginError> {
    let (action, mut inputs, separated) = {
        let page = Page::parse(text);
        let form = page.form(None).ok_or_else(|| {
            LoginError::LoginFailed(
                page.error_message()
                    .unwrap_or_else(|| "no sign-in form on the page".to_owned()),
            )
        })?;
        let separated = form
            .inputs
            .get("appAction")
            .is_some_and(|action| action.eq_ignore_ascii_case("SIGNIN_PWD_COLLECT"))
            || form
                .inputs
                .get("subPageType")
                .is_some_and(|sub| sub == "SignInClaimCollect");
        (form.action, form.inputs, separated)
    };
    inputs.insert("email".to_owned(), email.to_owned());
    inputs.insert("metadata1".to_owned(), metadata.to_owned());

    if !separated {
        inputs.insert("password".to_owned(), password.to_owned());
        return submit(http, base, &action, &inputs).await;
    }

    // Email first; then the returned page collects the password.
    inputs.remove("password");
    let (url, text) = submit(http, base, &action, &inputs).await?;
    let (action, mut inputs) = {
        let page = Page::parse(&text);
        let form = page.form(None).ok_or_else(|| {
            LoginError::LoginFailed(
                page.error_message()
                    .unwrap_or_else(|| "no password form on the page".to_owned()),
            )
        })?;
        (form.action, form.inputs)
    };
    inputs.insert("email".to_owned(), email.to_owned());
    inputs.insert("password".to_owned(), password.to_owned());
    inputs.insert("metadata1".to_owned(), metadata.to_owned());
    submit(http, &url, &action, &inputs).await
}

/// POSTs `inputs` to the form's (possibly relative) `action`, following
/// redirects, and returns the final URL + body.
async fn submit(
    http: &Client,
    base: &Url,
    action: &str,
    inputs: &BTreeMap<String, String>,
) -> Result<(Url, String), LoginError> {
    let target = base
        .join(action)
        .map_err(|_| LoginError::LoginFailed("invalid form action URL".to_owned()))?;
    let resp = http.post(target).form(inputs).send().await?;
    let url = resp.url().clone();
    let text = resp.text().await?;
    Ok((url, text))
}

/// Polls the approval page until the user approves elsewhere (or the budget
/// runs out), returning the page that no longer asks for approval.
async fn poll_approval(http: &Client, base: &Url) -> Result<(Url, String), LoginError> {
    let mut url = base.clone();
    for _ in 0..APPROVAL_POLL_MAX {
        let resp = http.get(url.clone()).send().await?;
        url = resp.url().clone();
        let text = resp.text().await?;
        if auth_code(&url).is_some() {
            return Ok((url, text));
        }
        let still_pending = {
            let page = Page::parse(&text);
            page.has_id("resend-approval-alert") || page.has_id("resend-approval-form")
        };
        if !still_pending {
            return Ok((url, text));
        }
        tokio::time::sleep(APPROVAL_POLL_INTERVAL).await;
    }
    Err(LoginError::LoginFailed(
        "approval was not confirmed in time".to_owned(),
    ))
}

/// Redirect policy for the sign-in client: follow redirects normally, but
/// **stop** as soon as a hop has carried the OAuth `authorization_code`.
///
/// The `br` marketplace answers `…/ap/maplanding?…authorization_code=…` with a
/// further HTTP 302 (to the Audible home page); reqwest's default policy would
/// follow it and drop the code from the final URL. Stopping once maplanding is
/// in `previous()` keeps `resp.url()` on the maplanding URL so [`auth_code`]
/// can read it. A no-op where maplanding is terminal (e.g. `de`). Shared with
/// the `account login server` proxy (AUD-60), whose upstream client needs the
/// same capture behaviour.
pub(crate) fn maplanding_redirect_policy() -> reqwest::redirect::Policy {
    const MAX_HOPS: usize = 10;
    reqwest::redirect::Policy::custom(|attempt| {
        let code_seen = attempt
            .previous()
            .iter()
            .any(|url| url.query().is_some_and(|q| q.contains(AUTH_CODE_PARAM)));
        if code_seen {
            attempt.stop()
        } else if attempt.previous().len() >= MAX_HOPS {
            attempt.error("too many redirects during sign-in")
        } else {
            attempt.follow()
        }
    })
}

/// The OAuth query parameter that carries the authorization code (one
/// home in the login module root, D6).
use super::AUTH_CODE_PARAM;

/// Optional debugging aid: when `AUDIBLE_LOGIN_DUMP_DIR` is set, write each
/// sign-in page's HTML there so a user can attach them to a bug report when
/// Amazon changes the page markup (which breaks form/challenge/message
/// detection). Page bodies carry **no password** — it is only ever sent, never
/// echoed — but they do contain session form tokens (and a masked phone/email
/// may show in visible text), so the printed hint asks the user to review
/// before sharing.
fn dump_page(step: usize, label: &str, text: &str) {
    let Ok(dir) = std::env::var("AUDIBLE_LOGIN_DUMP_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("login-{step:02}-{label}.html"));
    match std::fs::write(&path, text) {
        Ok(()) => tracing::warn!(
            "login: dumped sign-in page to {} \
             (contains session form tokens — review before sharing)",
            path.display()
        ),
        Err(error) => tracing::warn!(%error, "could not write the login page dump"),
    }
}

/// Extracts `openid.oa2.authorization_code` from a (redirected) URL.
pub(crate) fn auth_code(url: &Url) -> Option<String> {
    super::auth_code_from_pairs(url.query_pairs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn login_client() -> Client {
        Client::builder()
            .redirect(maplanding_redirect_policy())
            .build()
            .unwrap()
    }

    /// `br`-style: maplanding carries the code, then 302s onward. The policy
    /// must stop on the maplanding hop so the code survives in `resp.url()`.
    #[tokio::test]
    async fn stops_on_maplanding_when_it_redirects_onward() {
        let server = MockServer::start().await;
        let maplanding = format!(
            "{}/ap/maplanding?openid.oa2.authorization_code=CODE123",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", maplanding.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ap/maplanding"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/home", server.uri()).as_str()),
            )
            .mount(&server)
            .await;

        let resp = login_client()
            .get(format!("{}/start", server.uri()))
            .send()
            .await
            .unwrap();

        assert!(resp.url().path().ends_with("/ap/maplanding"));
        assert_eq!(auth_code(resp.url()).as_deref(), Some("CODE123"));
    }

    /// `de`-style: maplanding is terminal (200). The code is already in the
    /// final URL — the policy is a no-op.
    #[tokio::test]
    async fn keeps_code_when_maplanding_is_terminal() {
        let server = MockServer::start().await;
        let maplanding = format!(
            "{}/ap/maplanding?openid.oa2.authorization_code=DE456",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", maplanding.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ap/maplanding"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let resp = login_client()
            .get(format!("{}/start", server.uri()))
            .send()
            .await
            .unwrap();

        assert_eq!(auth_code(resp.url()).as_deref(), Some("DE456"));
    }
}
