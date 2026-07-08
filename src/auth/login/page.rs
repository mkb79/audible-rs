//! Minimal HTML helper for the scripted login — a `SoupPage`-equivalent over
//! `scraper`. Extracts a form's action + inputs and reads Amazon's auth
//! message boxes, plus small predicates for challenge detection.
//!
//! All accessors return **owned** data so a caller can parse a page, take what
//! it needs, and drop the (`!Send`) [`scraper::Html`] before any `.await`.

use std::collections::BTreeMap;

use scraper::{ElementRef, Html, Selector};

/// A parsed login page.
pub(crate) struct Page {
    html: Html,
}

/// An extracted HTML form: its submit target and input name→value pairs
/// (hidden inputs keep their value; others are empty, ready to be filled). The
/// login flow always POSTs, so the form method is not modelled.
pub(crate) struct Form {
    pub action: String,
    pub inputs: BTreeMap<String, String>,
}

impl Page {
    pub fn parse(text: &str) -> Self {
        Self {
            html: Html::parse_document(text),
        }
    }

    /// Finds a form by `name=<name>` (falling back to the default sign-in form
    /// `form[name=signIn]`, then the first form) and extracts it.
    pub fn form(&self, name: Option<&str>) -> Option<Form> {
        let form_el = self.find_form(name)?;
        let action = form_el.value().attr("action")?.to_owned();

        let input = selector("input");
        let mut inputs = BTreeMap::new();
        for field in form_el.select(&input) {
            let Some(field_name) = field.value().attr("name") else {
                continue;
            };
            let value = if field.value().attr("type") == Some("hidden") {
                field.value().attr("value").unwrap_or("").to_owned()
            } else {
                String::new()
            };
            inputs.insert(field_name.to_owned(), value);
        }
        Some(Form { action, inputs })
    }

    fn find_form(&self, name: Option<&str>) -> Option<ElementRef<'_>> {
        if let Some(name) = name
            && let Ok(by_name) = Selector::parse(&format!("form[name={name:?}]"))
            && let Some(form) = self.html.select(&by_name).next()
        {
            return Some(form);
        }
        if let Some(form) = self.html.select(&selector("form[name=\"signIn\"]")).next() {
            return Some(form);
        }
        self.html.select(&selector("form")).next()
    }

    /// A page error message, if any: the legacy `auth-error-message-box` /
    /// `ap_error_page_message`, or a visible Amazon `a-alert-error`.
    pub fn error_message(&self) -> Option<String> {
        self.message_box("auth-error-message-box")
            .or_else(|| self.message_box("ap_error_page_message"))
            .or_else(|| self.alert_message("a-alert-error"))
    }

    /// A page warning/notice, if any: the legacy `auth-warning-message-box`, or
    /// a visible Amazon `a-alert-warning` / `a-alert-info` (e.g. the OTP page's
    /// "we are unable to send an SMS to …").
    pub fn warning_message(&self) -> Option<String> {
        self.message_box("auth-warning-message-box")
            .or_else(|| self.alert_message("a-alert-warning"))
            .or_else(|| self.alert_message("a-alert-info"))
    }

    /// The text of the first **visible** `a-alert` with the given modifier
    /// class. Amazon's auth pages use the `a-alert` component for messages
    /// (`<div class="a-alert a-alert-warning"> … <div class="a-alert-content">`)
    /// and sprinkle hidden client-side validation templates (`aok-hidden`)
    /// throughout — those are skipped, as are empty ones.
    fn alert_message(&self, modifier: &str) -> Option<String> {
        let alerts = Selector::parse(&format!("div.{modifier}")).ok()?;
        let content = selector("div.a-alert-content");
        for alert in self.html.select(&alerts) {
            if alert.value().classes().any(|class| class == "aok-hidden") {
                continue;
            }
            let Some(node) = alert.select(&content).next() else {
                continue;
            };
            let text = collapse(&node.text().collect::<String>());
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
        None
    }

    fn message_box(&self, id: &str) -> Option<String> {
        let box_el = self.by_id(id)?;
        let mut message = String::new();
        if let Some(header) = box_el.select(&selector("h4")).next() {
            message.push_str(collapse(&header.text().collect::<String>()).trim());
        }
        for item in box_el.select(&selector("li span")) {
            let text = item.text().collect::<String>();
            let text = collapse(&text);
            if !text.trim().is_empty() {
                if !message.is_empty() {
                    message.push(' ');
                }
                message.push_str(text.trim());
            }
        }
        if message.trim().is_empty() {
            let fallback = collapse(&box_el.text().collect::<String>());
            let fallback = fallback.trim();
            (!fallback.is_empty()).then(|| fallback.to_owned())
        } else {
            Some(message.trim().to_owned())
        }
    }

    /// Whether an `<input name=...>` exists (challenge detection).
    pub fn has_input(&self, name: &str) -> bool {
        Selector::parse(&format!("input[name={name:?}]"))
            .ok()
            .is_some_and(|sel| self.html.select(&sel).next().is_some())
    }

    /// Whether an element with the given id exists.
    pub fn has_id(&self, id: &str) -> bool {
        self.by_id(id).is_some()
    }

    /// An attribute of the element with the given id (e.g. a captcha `src`).
    pub fn attr_by_id(&self, id: &str, attr: &str) -> Option<String> {
        self.by_id(id)?.value().attr(attr).map(str::to_owned)
    }

    fn by_id(&self, id: &str) -> Option<ElementRef<'_>> {
        let sel = Selector::parse(&format!("#{id}")).ok()?;
        self.html.select(&sel).next()
    }

    /// The captcha image URL, if this is a captcha page.
    pub fn captcha_image_url(&self) -> Option<String> {
        if let Some(src) = self.attr_by_id("auth-captcha-image", "src") {
            return Some(src);
        }
        self.html
            .select(&selector("img[src*=\"captcha\"]"))
            .next()?
            .value()
            .attr("src")
            .map(str::to_owned)
    }

    /// The MFA device options `(value, label, method)` of a device-picker page.
    pub fn mfa_devices(&self) -> Vec<(String, String, String)> {
        let node_sel = selector("div[data-a-input-name=\"otpDeviceContext\"]");
        let radio_sel = selector("input[type=\"radio\"]");
        let label_sel = selector("span.a-label");
        let mut out = Vec::new();
        for node in self.html.select(&node_sel) {
            let value = node
                .select(&radio_sel)
                .next()
                .and_then(|radio| radio.value().attr("value"))
                .unwrap_or("")
                .to_owned();
            if value.is_empty() {
                continue;
            }
            let label = node
                .select(&label_sel)
                .next()
                .map(|label| collapse(&label.text().collect::<String>()))
                .unwrap_or_default();
            let method = node
                .value()
                .classes()
                .find(|class| class.starts_with("auth-"))
                .map(|class| class.trim_start_matches("auth-").to_owned())
                .unwrap_or_default();
            out.push((value, label, method));
        }
        out
    }
}

/// Parses a static selector, panicking on a malformed literal (programmer error).
fn selector(s: &str) -> Selector {
    Selector::parse(s).expect("valid static selector")
}

/// Collapses runs of whitespace to single spaces (HTML text is whitespace-noisy).
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNIN: &str = r#"
        <html><body>
        <form name="signIn" method="post" action="https://www.amazon.de/ap/signin">
          <input type="hidden" name="appActionToken" value="tok123"/>
          <input type="hidden" name="appAction" value="SIGNIN_PWD_COLLECT"/>
          <input type="email" name="email"/>
          <input type="password" name="password"/>
        </form>
        <div id="auth-error-message-box"><h4>There was a problem</h4>
          <ul><li><span>Your password is incorrect</span></li></ul></div>
        </body></html>"#;

    #[test]
    fn extracts_form_action_and_inputs() {
        let page = Page::parse(SIGNIN);
        let form = page.form(None).unwrap();
        assert_eq!(form.action, "https://www.amazon.de/ap/signin");
        assert_eq!(form.inputs.get("appActionToken").unwrap(), "tok123");
        assert_eq!(form.inputs.get("appAction").unwrap(), "SIGNIN_PWD_COLLECT");
        // Non-hidden inputs are present but empty (to be filled).
        assert_eq!(form.inputs.get("email").unwrap(), "");
        assert_eq!(form.inputs.get("password").unwrap(), "");
    }

    #[test]
    fn reads_error_message_box() {
        let page = Page::parse(SIGNIN);
        assert_eq!(
            page.error_message().as_deref(),
            Some("There was a problem Your password is incorrect")
        );
        assert!(page.warning_message().is_none());
    }

    #[test]
    fn detects_inputs_and_ids() {
        let page = Page::parse(SIGNIN);
        assert!(page.has_input("password"));
        assert!(!page.has_input("otpCode"));
        assert!(page.has_id("auth-error-message-box"));
        assert!(!page.has_id("auth-captcha-image"));
    }

    #[test]
    fn reads_a_alert_warning_and_skips_hidden_templates() {
        // The OTP page's notice is an `a-alert-warning`; the page also carries
        // hidden client-side validation templates that must NOT be surfaced.
        let html = r#"
            <div class="a-box a-alert a-alert-error aok-hidden">
              <div class="a-alert-container"><div class="a-alert-content">hidden error template</div></div>
            </div>
            <div class="a-box a-alert a-alert-warning">
              <div class="a-alert-container"><div class="a-alert-content">
                <h4 class="a-alert-heading">Heads up</h4>
                We are unable to send an SMS to the number ending 964.
              </div></div>
            </div>"#;
        let page = Page::parse(html);
        assert_eq!(
            page.warning_message().as_deref(),
            Some("Heads up We are unable to send an SMS to the number ending 964.")
        );
        // The only a-alert-error is hidden, so no error is surfaced.
        assert_eq!(page.error_message(), None);
    }

    #[test]
    fn reads_visible_a_alert_error() {
        let html = r#"<div class="a-alert a-alert-error">
          <div class="a-alert-content">That code was incorrect.</div></div>"#;
        let page = Page::parse(html);
        assert_eq!(
            page.error_message().as_deref(),
            Some("That code was incorrect.")
        );
    }
}
