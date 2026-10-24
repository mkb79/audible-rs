//! `account login` and `account login server`: the live sign-in flows
//! plus their terminal UI (the device-code prompt, challenge prompts, QR
//! code, captcha display).

use anyhow::{Result, bail};
use clap::Args;
use zeroize::Zeroizing;

use crate::api::locale::{self, Locale};
use crate::auth::Authenticator;
use crate::auth::device::{Device, DeviceKind};
use crate::auth::login as login_flow;
use crate::config::ctx::Ctx;

use super::*;

/// Register a new account via a live sign-in. Default: the device-code
/// sign-in (mkb79/Audible#906) — a short code you confirm at
/// `amazon.<tld>/code` in any browser; your password never passes through
/// this program. `--external` prints the sign-in URL for your browser and
/// reads back the redirect — the flow pre-merger Audible accounts
/// (`--audible-username`, DE/US/UK) always take, as they have no Amazon
/// sign-in. `--password` is the deprecated scripted login that asks for
/// email + password here and answers challenges in the terminal — kept only
/// as a fallback in case the device code does not work everywhere.
#[derive(Debug, Args)]
pub(super) struct LoginArgs {
    /// Use the browser-URL flow instead of the device code: print the sign-in
    /// URL for your browser, paste the redirect back here
    #[arg(long, alias = "link", help_heading = "Sign-in method")]
    external: bool,

    /// Deprecated: type your email + password here (the scripted login, with
    /// CAPTCHA/OTP prompts in the terminal). Only a fallback in case the
    /// device code does not work for you; breaks whenever Amazon changes its
    /// sign-in pages
    #[arg(
        long,
        alias = "scripted",
        conflicts_with = "external",
        help_heading = "Sign-in method"
    )]
    password: bool,

    /// Sign in with an Audible username (pre-merger accounts) instead of an
    /// Amazon email; routes the browser-URL flow to the Audible domain. DE, US
    /// and UK only
    #[arg(long, alias = "with-username", help_heading = "Sign-in method")]
    audible_username: bool,

    #[command(flatten)]
    reg: RegistrationArgs,
}

/// Registration inputs shared by `login` and `login server` (flattened
/// into both, so the flags and their help exist once): device choice,
/// account naming, the marketplace axis and the auth-file options.
#[derive(Debug, Args)]
struct RegistrationArgs {
    /// Device to register as: iphone (default) or android
    #[arg(long, value_name = "DEVICE", help_heading = "Registration")]
    device: Option<String>,

    /// Account name (default: asked interactively)
    #[arg(long, help_heading = "Registration")]
    name: Option<String>,

    /// Marketplaces this account owns audiobooks on (CSV of country codes),
    /// saved for later data commands like `library sync`/`list`. The sign-in
    /// still registers a single device on one marketplace — this adds no extra
    /// registrations. Default: the registration marketplace
    #[arg(long, value_name = "CC,...", help_heading = "Registration")]
    marketplaces: Option<String>,

    /// Of --marketplaces, the subset the global -m defaults to when it is
    /// omitted on later commands (must be a subset). Default: the registration
    /// marketplace
    #[arg(long, value_name = "CC,...", help_heading = "Registration")]
    default_marketplaces: Option<String>,

    /// Write the new auth file unencrypted (not recommended)
    #[arg(long, help_heading = "Auth file")]
    plain: bool,

    /// Overwrite an existing auth file and config entry
    #[arg(long, help_heading = "Auth file")]
    force: bool,
}

/// Log in through a local browser-proxy server (headless-friendly)
#[derive(Debug, Args)]
pub(super) struct ServerArgs {
    /// Sign in with an Audible username (pre-merger accounts); DE/US/UK only
    #[arg(long, alias = "with-username", help_heading = "Sign-in method")]
    audible_username: bool,

    /// Address to bind. Default 127.0.0.1 (loopback — reach it via an SSH
    /// forward). Pass a reachable IP (or 0.0.0.0) to open it from a phone
    #[arg(long, default_value = "127.0.0.1", help_heading = "Server")]
    host: String,

    /// Port to bind. Default 0 picks a free ephemeral port
    #[arg(long, default_value_t = 0, help_heading = "Server")]
    port: u16,

    /// Seconds to wait for the browser sign-in before giving up
    #[arg(long, default_value_t = 300, help_heading = "Server")]
    timeout: u64,

    #[command(flatten)]
    reg: RegistrationArgs,
}

/// Per-request timeout of the device-code HTTP client: a stalled poll is
/// retried like any other failure instead of hanging past the code's expiry.
const DEVICE_CODE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) async fn login(ctx: &Ctx, args: LoginArgs) -> Result<()> {
    // The registration marketplace comes from -m (a single cc) — there is no
    // account yet to resolve a marketplace set against.
    let cc = ctx.marketplace_selector().ok_or_else(|| {
        anyhow::anyhow!("specify the registration marketplace with -m <cc> (e.g. -m de)")
    })?;
    if cc.contains(',') || cc.eq_ignore_ascii_case("all") {
        bail!("account login registers in a single marketplace; pass one -m <cc> (e.g. -m de)");
    }
    let locale = locale::require(&cc).map_err(|error| anyhow::anyhow!(error))?;

    let kind = match &args.reg.device {
        Some(value) => value
            .parse::<DeviceKind>()
            .map_err(|err| anyhow::anyhow!(err))?,
        None => DeviceKind::DEFAULT,
    };
    let device = Device::generate(kind);

    if args.audible_username && !login_flow::username_login_supported(&locale) {
        bail!(
            "username (pre-merger Audible) login is only available for the de, us and uk \
             marketplaces"
        );
    }

    let auth = if args.password {
        ctx.warn(
            "password_login_deprecated",
            "--password is deprecated: the scripted login is only a fallback in case \
             the device code does not work for you (run `audible account login` \
             without it)",
        );
        login_scripted(&locale, &device, args.audible_username).await?
    } else if args.external || args.audible_username {
        if !args.external {
            // A pre-merger account has no Amazon sign-in to confirm a device
            // code with (mkb79/Audible#906): the browser-URL flow is its one.
            eprintln!("Pre-merger Audible accounts sign in through the browser-URL flow.\n");
        }
        login_external(&locale, &device, args.audible_username).await?
    } else {
        login_with_device_code(&locale, &device).await?
    };
    // Amazon assigns the device name (e.g. "Alice's 4th Audible for iPhone");
    // it is stored in the auth file and shown so the device can be removed.
    let device_name = auth.device_name().map(str::to_owned);

    let (name, _) = finalize_account(
        ctx,
        Registration {
            auth,
            name: args.reg.name,
            default_name: locale.country_code.to_owned(),
            marketplaces: args.reg.marketplaces,
            default_marketplaces: args.reg.default_marketplaces,
            plain: args.reg.plain,
            force: args.reg.force,
        },
    )
    .await?;
    eprintln!(
        "Logged in and registered account {name:?} ({}).",
        locale.country_code
    );
    if let Some(device_name) = device_name {
        eprintln!(
            "Amazon registered this device as {device_name:?} — remove it later under \
             \"Manage Your Content and Devices\" in your Amazon account if needed."
        );
    }
    Ok(())
}

/// The device-code sign-in (the default): asks Amazon for a code, shows it
/// together with the page to enter it on, and waits until the sign-in there
/// has linked the device. Nothing is typed here but the account name.
async fn login_with_device_code(locale: &Locale, device: &Device) -> Result<Authenticator> {
    let http = reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .timeout(DEVICE_CODE_REQUEST_TIMEOUT)
        .build()?;
    let code_pair = login_flow::create_code_pair(&http, locale, device).await?;
    let minutes = code_pair.time_left().as_secs().div_ceil(60).max(1);
    eprintln!(
        "Open this page in a browser on any device, sign in with your Amazon account and \
         enter the code shown below:\n\n  {}\n\n  Code: {}\n",
        login_flow::verification_url(locale),
        code_pair.public_code()
    );
    eprintln!(
        "The code is valid for about {minutes} minutes. Waiting for the sign-in to \
         complete… (press Ctrl-C to abort)"
    );
    Ok(login_flow::register_with_code_pair(&http, locale, device, &code_pair).await?)
}

/// The browser-URL flow: prints the PKCE authorize URL, reads the pasted
/// redirect back and registers with its authorization code (no password is
/// entered here).
async fn login_external(
    locale: &Locale,
    device: &Device,
    with_username: bool,
) -> Result<Authenticator> {
    let pkce = login_flow::Pkce::generate();
    let url = login_flow::authorize_url(device, &pkce, locale, with_username);
    eprintln!(
        "Open this URL in your browser and sign in. When the page goes blank \
         (a \"page not found\" at .../ap/maplanding), copy the full address-bar URL:\n\n{url}\n"
    );
    let code = read_authorization_code()?;
    let http = reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .build()?;
    Ok(login_flow::register(&http, locale, device, &pkce, &code, with_username).await?)
}

/// Asks for the URL the browser ended on until it holds an authorization
/// code (a few attempts). Terminals may cut off a long pasted line, so the
/// path of a text file holding the URL is accepted as well.
fn read_authorization_code() -> Result<String> {
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let answer =
            prompt_required("Paste the redirect URL, or the path of a text file containing it")?;
        match authorization_code_from_answer(&answer) {
            Ok(code) => return Ok(code),
            Err(error) if attempt < ATTEMPTS => eprintln!("{error} — please try again."),
            Err(error) => return Err(error),
        }
    }
    unreachable!("the last attempt returns")
}

/// The authorization code from a pasted redirect URL, or from the file the
/// answer names when it is not a URL.
fn authorization_code_from_answer(answer: &str) -> Result<String> {
    let redirect = if answer.trim().to_ascii_lowercase().starts_with("http") {
        answer.to_owned()
    } else {
        std::fs::read_to_string(answer.trim())
            .map_err(|error| anyhow::anyhow!("cannot read {:?}: {error}", answer.trim()))?
            .trim_start_matches('\u{feff}')
            .to_owned()
    };
    Ok(login_flow::extract_authorization_code(&redirect)?)
}

/// The deprecated scripted login: email + password typed here, challenges
/// answered in the terminal. Pre-merger accounts sign in with an Audible
/// username + password instead.
async fn login_scripted(
    locale: &Locale,
    device: &Device,
    with_username: bool,
) -> Result<Authenticator> {
    let pkce = login_flow::Pkce::generate();
    let (id_label, pw_label) = if with_username {
        ("Audible username", "Audible password")
    } else {
        ("Amazon email", "Amazon password")
    };
    let username = prompt_required(id_label)?;
    let password = Zeroizing::new(prompt_secret(pw_label)?);
    eprintln!("Signing in to the {} marketplace…", locale.country_code);
    Ok(login_flow::login_internal(
        locale,
        device,
        &pkce,
        &username,
        &password,
        &TerminalPrompt,
        with_username,
    )
    .await?)
}

/// `account login server`: runs a local reverse-proxy so a real browser (incl.
/// a phone via QR) completes the sign-in while we capture the code. The user
/// first picks the marketplace/device/name/pre-merger on a small config page;
/// the CLI flags only pre-fill it. An alternative to the default device code
/// and the browser-URL flow, and a clean path for the `br` redirect.
pub(super) async fn login_server(ctx: &Ctx, args: ServerArgs) -> Result<()> {
    // The marketplace is chosen on the config page; -m (a single cc) only
    // pre-selects it.
    let default_cc = ctx
        .marketplace_selector()
        .filter(|cc| !cc.contains(',') && !cc.eq_ignore_ascii_case("all"));
    let device_default = match &args.reg.device {
        Some(value) => value
            .parse::<DeviceKind>()
            .map_err(|err| anyhow::anyhow!(err))?,
        None => DeviceKind::DEFAULT,
    };

    let ip: std::net::IpAddr = args.host.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid --host {:?} (an IP like 127.0.0.1 or 0.0.0.0)",
            args.host
        )
    })?;
    let addr = std::net::SocketAddr::new(ip, args.port);

    let defaults = login_flow::LoginDefaults {
        country_code: default_cc,
        device: device_default,
        username: args.audible_username,
        name: args.reg.name.clone(),
        marketplaces: args.reg.marketplaces.clone(),
        default_marketplaces: args.reg.default_marketplaces.clone(),
        plain: args.reg.plain,
    };
    let server = login_flow::LoginServer::bind(addr, defaults).await?;
    let port = server.local_port();
    let path = server.landing_path();

    eprintln!("Sign-in server listening on {ip}:{port}.");
    if ip.is_unspecified() {
        eprintln!(
            "Bound to a wildcard address — open  http://<this-host-ip>:{port}{path}\n\
             (pass --host <reachable-ip> instead of {} to also print a QR code)\n",
            args.host
        );
    } else if ip.is_loopback() {
        let url = format!("http://{}:{port}{path}", args.host);
        eprintln!("Open this URL in a browser on THIS machine:\n\n  {url}\n");
        eprintln!(
            "On a headless box, forward the port first, then open the URL on your laptop:\n  \
             ssh -L {port}:localhost:{port} <user>@<this-host>\n"
        );
    } else {
        let url = format!("http://{}:{port}{path}", args.host);
        eprintln!(
            "\u{26a0} A non-loopback bind briefly proxies an Amazon sign-in on your network — \
             it shuts down after login or timeout."
        );
        eprintln!("Open this URL in a browser (or scan the QR with your phone):\n\n  {url}\n");
        print_qr(&url);
    }
    eprintln!(
        "Waiting up to {}s for the sign-in to complete… (press Ctrl-C to abort)",
        args.timeout
    );

    let login = server
        .run(std::time::Duration::from_secs(args.timeout))
        .await?;
    let http = reqwest::Client::builder()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .build()?;
    let auth = login_flow::register(
        &http,
        &login.locale,
        &login.device,
        &login.pkce,
        &login.code,
        login.with_username,
    )
    .await?;
    let device_name = auth.device_name().map(str::to_owned);
    let country_code = login.locale.country_code.to_owned();

    let (name, _) = finalize_account(
        ctx,
        Registration {
            auth,
            name: login.name.or(args.reg.name),
            default_name: country_code.clone(),
            marketplaces: login.marketplaces,
            default_marketplaces: login.default_marketplaces,
            plain: login.plain,
            force: args.reg.force,
        },
    )
    .await?;
    eprintln!("Logged in and registered account {name:?} ({country_code}).");
    if let Some(device_name) = device_name {
        eprintln!(
            "Amazon registered this device as {device_name:?} — remove it later with \
             `account logout` if needed."
        );
    }
    Ok(())
}

/// Renders `url` as a compact QR code on stderr (for the phone/Smart-TV flow).
fn print_qr(url: &str) {
    use qrcode::QrCode;
    use qrcode::render::unicode;
    if let Ok(code) = QrCode::new(url.as_bytes()) {
        let rendered = code.render::<unicode::Dense1x2>().quiet_zone(true).build();
        eprintln!("{rendered}\n");
    }
}

/// Terminal implementation of the login challenge prompts (the UI side of the
/// UI-free login core).
struct TerminalPrompt;

impl login_flow::ChallengePrompt for TerminalPrompt {
    fn notice(&self, message: &str) {
        // Set page warnings/notices off from the surrounding prompts with a
        // colour and blank lines, so they don't run into the next question.
        let term = console::Term::stderr();
        let _ = term.write_line(&format!("\n{}\n", console::style(message).yellow()));
    }

    fn captcha(&self, image_url: &str, image: &[u8]) -> Result<String, login_flow::LoginError> {
        display_captcha(image_url, image);
        read_challenge("CAPTCHA (type the characters shown)")
    }

    fn otp(&self) -> Result<String, login_flow::LoginError> {
        read_challenge("OTP / 2FA code")
    }

    fn cvf(&self) -> Result<String, login_flow::LoginError> {
        read_challenge("Verification code (sent to your email/phone)")
    }

    fn mfa_choice(
        &self,
        devices: &[login_flow::MfaDevice],
    ) -> Result<usize, login_flow::LoginError> {
        let term = console::Term::stderr();
        let _ = term.write_line("Where should the code be sent?");
        for (index, device) in devices.iter().enumerate() {
            let label = if device.label.is_empty() {
                device.method.as_str()
            } else {
                device.label.as_str()
            };
            let _ = term.write_line(&format!("  {}) {label} [{}]", index + 1, device.method));
        }
        loop {
            let _ = term.write_str("Device number: ");
            let line = term
                .read_line()
                .map_err(|_| login_flow::LoginError::Cancelled)?;
            if let Ok(choice) = line.trim().parse::<usize>()
                && (1..=devices.len()).contains(&choice)
            {
                return Ok(choice - 1);
            }
        }
    }

    fn approval(&self) -> Result<(), login_flow::LoginError> {
        let term = console::Term::stderr();
        let _ = term.write_line(
            "Amazon sent an approval notification to your email/app. \
             Approve it there, then press ENTER to continue…",
        );
        term.read_line()
            .map(|_| ())
            .map_err(|_| login_flow::LoginError::Cancelled)
    }
}

/// Prompts (stderr) for a challenge answer; empty input cancels.
fn read_challenge(label: &str) -> Result<String, login_flow::LoginError> {
    let term = console::Term::stderr();
    term.write_str(&format!("{label}: "))
        .map_err(|_| login_flow::LoginError::Cancelled)?;
    let line = term
        .read_line()
        .map_err(|_| login_flow::LoginError::Cancelled)?;
    let answer = line.trim();
    if answer.is_empty() {
        Err(login_flow::LoginError::Cancelled)
    } else {
        Ok(answer.to_owned())
    }
}

/// Shows the captcha: saves it to a temp file + prints the URL, and renders it
/// inline on iTerm2-family terminals (no image-decode dependency).
fn display_captcha(image_url: &str, image: &[u8]) {
    use base64::Engine as _;

    let term = console::Term::stderr();
    let path = std::env::temp_dir().join("audible-captcha.img");
    let saved = std::fs::write(&path, image).is_ok();

    if iterm2_inline_supported() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(image);
        let _ = term.write_str(&format!(
            "\x1b]1337;File=inline=1;size={};width=auto;height=auto;preserveAspectRatio=1:{encoded}\x07\n",
            image.len()
        ));
    }
    let _ = term.write_line(&format!("CAPTCHA image: {image_url}"));
    if saved {
        let _ = term.write_line(&format!("(also saved to {})", path.display()));
    }
}

/// Whether the terminal supports the iTerm2 inline-image protocol.
fn iterm2_inline_supported() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|value| matches!(value.as_str(), "iTerm.app" | "WezTerm" | "ghostty"))
        .unwrap_or(false)
        || std::env::var("LC_TERMINAL")
            .map(|value| value == "iTerm2")
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_code_comes_from_a_url_or_a_file() {
        let url = "https://www.audible.de/ap/maplanding?openid.oa2.authorization_code=CODE1&x=y";
        assert_eq!(authorization_code_from_answer(url).unwrap(), "CODE1");
        assert_eq!(
            authorization_code_from_answer(&format!("  {url}\n")).unwrap(),
            "CODE1"
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("redirect.txt");
        std::fs::write(&file, format!("\u{feff}{url}\n")).unwrap();
        assert_eq!(
            authorization_code_from_answer(file.to_str().unwrap()).unwrap(),
            "CODE1"
        );

        let missing = dir.path().join("missing.txt");
        let error = authorization_code_from_answer(missing.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().starts_with("cannot read"), "{error}");
        assert!(authorization_code_from_answer("https://www.audible.de/ap/maplanding").is_err());
    }
}
