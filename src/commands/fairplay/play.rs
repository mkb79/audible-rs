//! `audible fairplay stream` and `audible fairplay play`: online playback
//! with a streaming key per request, and offline playback of a downloaded
//! package with its stored keys. Both play through AVPlayer on the main
//! thread until the end, `--duration`, or Ctrl-C.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Arg;

use super::store::Store;
use crate::config::ctx::Ctx;
use crate::fairplay::driver::{self, PlayEvent, PlaySource, Playback};
use crate::fairplay::{self, Consumption};

fn playback_args() -> [Arg; 3] {
    [
        Arg::new("start")
            .long("start")
            .value_name("SECS")
            .value_parser(clap::value_parser!(f64))
            .conflicts_with("exact_start")
            .help(
                "Start position in seconds; jumps to the segment boundary before it (fast, \
                 a few seconds early)",
            ),
        Arg::new("exact_start")
            .long("exact-start")
            .value_name("SECS")
            .value_parser(clap::value_parser!(f64))
            .help("Start exactly at this position in seconds (takes a moment longer)"),
        Arg::new("duration")
            .long("duration")
            .value_name("SECS")
            .value_parser(clap::value_parser!(f64))
            .help("Stop after this many seconds (default: play to the end)"),
    ]
}

pub(super) fn stream_command() -> clap::Command {
    let cmd = clap::Command::new("stream")
        .about("Stream a title through FairPlay (experimental, macOS)")
        .long_about(
            "Play a title online through FairPlay (experimental, macOS only): a streaming \
             license, a key per request, nothing stored. Stops at the end, after --duration, \
             or on Ctrl-C.",
        );
    crate::commands::items::item_source_args(cmd)
        .arg(
            Arg::new("allow_external")
                .long("allow-external")
                .action(clap::ArgAction::SetTrue)
                .help("Allow an --asin that is not in your library"),
        )
        .args(super::codec_args())
        .args(playback_args())
}

pub(super) fn play_command() -> clap::Command {
    let cmd = clap::Command::new("play")
        .about("Play a FairPlay download offline (experimental, macOS)")
        .long_about(
            "Play a title downloaded with `audible fairplay download`, offline, with its \
             stored keys (experimental, macOS only). Stops at the end, after --duration, or \
             on Ctrl-C.",
        );
    crate::commands::items::item_source_args(cmd)
        .arg(super::dir_arg())
        .args(playback_args())
}

fn playback(matches: &clap::ArgMatches) -> Playback {
    let exact = matches.get_one::<f64>("exact_start").copied();
    Playback {
        start: exact
            .or_else(|| matches.get_one::<f64>("start").copied())
            .unwrap_or(0.0),
        exact: exact.is_some(),
        duration: matches.get_one::<f64>("duration").copied(),
    }
}

pub(super) async fn stream(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let marketplace = ctx.marketplace_single()?;
    let asin = super::single_title(
        ctx,
        &marketplace,
        matches,
        !matches.get_flag("allow_external"),
    )
    .await?;
    let client = ctx.client().await?;
    let options = super::license_options(matches, Consumption::Streaming);
    let grant = fairplay::request_license(client, &marketplace, &asin, &options).await?;
    let certificate = match &grant.certificate {
        Some(certificate) => certificate.clone(),
        None => fairplay::request_certificate(client, &marketplace).await?,
    };
    let title = super::title_of(ctx, &marketplace, &asin).await;
    let mut status = Status::new(title.as_deref().unwrap_or(&asin), matches);
    let handle = tokio::runtime::Handle::current();
    let stop = super::stop_flag();
    let result = tokio::task::block_in_place(|| {
        let mut ckc = super::ckc_exchange(
            &handle,
            client,
            &marketplace,
            &asin,
            grant.license_id.as_deref(),
            Consumption::Streaming,
        );
        driver::play(
            PlaySource::Stream {
                hls_url: &grant.hls_url,
                certificate: &certificate,
            },
            &playback(matches),
            &mut ckc,
            &mut |event| status.on(event),
            &|| stop.load(Ordering::Relaxed),
        )
    });
    status.finish();
    result.map_err(|error| anyhow!("{asin}: {error}"))
}

pub(super) async fn play(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let marketplace = ctx.marketplace_single()?;
    let store = Store::open(ctx, matches.get_one::<String>("dir"))?;
    let index = store.load()?;
    // The store decides what can play, so any --asin is fine here.
    let asin = super::single_title(ctx, &marketplace, matches, false).await?;
    let entry = index.get(&marketplace, &asin).cloned().with_context(|| {
        format!(
            "{asin} has no FairPlay download — get one with \
             `audible fairplay download --asin {asin}`"
        )
    })?;
    if !entry.package.exists() {
        bail!(
            "the package of {asin} is gone (macOS may have purged it) — download it again \
             with `audible fairplay download --asin {asin} --force`"
        );
    }
    let mut keys = store.read_keys(&entry.keys)?;
    let mut status = Status::new(entry.title.as_deref().unwrap_or(&asin), matches);
    let mut renewed: HashMap<String, Vec<u8>> = HashMap::new();
    let stop = super::stop_flag();
    // Offline: the stored keys answer every request, so nothing is sent.
    let mut ckc = |_: &str, _: &[u8]| -> Result<Vec<u8>, String> {
        Err("offline playback sends no license request".into())
    };
    let result = tokio::task::block_in_place(|| {
        driver::play(
            PlaySource::Package {
                path: &entry.package,
                keys: &keys,
            },
            &playback(matches),
            &mut ckc,
            &mut |event| match event {
                PlayEvent::KeyUpdated { content_id, key } => {
                    renewed.insert(content_id, key);
                }
                other => status.on(other),
            },
            &|| stop.load(Ordering::Relaxed),
        )
    });
    status.finish();
    if !renewed.is_empty() {
        keys.extend(renewed);
        store.write_keys(&entry.keys, &keys)?;
    }
    result.map_err(|error| anyhow!("{asin}: {error}"))
}

/// The position line on an interactive stderr (`title  0:01:23 / 5:12:00`).
struct Status {
    label: String,
    shown: bool,
    duration: Option<f64>,
    printed: bool,
}

impl Status {
    fn new(label: &str, matches: &clap::ArgMatches) -> Self {
        Self {
            label: label.to_owned(),
            shown: !matches.get_flag("quiet") && console::Term::stderr().is_term(),
            duration: None,
            printed: false,
        }
    }

    fn on(&mut self, event: PlayEvent) {
        match event {
            PlayEvent::Ready { duration } => self.duration = duration,
            PlayEvent::Position(position) if self.shown => {
                let total = self
                    .duration
                    .map(super::clock)
                    .unwrap_or_else(|| "?".to_owned());
                eprint!("\r{}  {} / {total}   ", self.label, super::clock(position));
                self.printed = true;
            }
            PlayEvent::Position(_) | PlayEvent::KeyUpdated { .. } | PlayEvent::Ended => {}
        }
    }

    fn finish(&self) {
        if self.printed {
            eprintln!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        clap::Command::new("t")
            .args(playback_args())
            .try_get_matches_from(std::iter::once("t").chain(args.iter().copied()))
    }

    #[test]
    fn start_and_exact_start_exclude_each_other() {
        assert!(parse(&["--start", "10", "--exact-start", "20"]).is_err());
    }

    #[test]
    fn playback_follows_the_start_flags() {
        let default = playback(&parse(&[]).unwrap());
        assert_eq!((default.start, default.exact), (0.0, false));
        let fast = playback(&parse(&["--start", "3600"]).unwrap());
        assert_eq!((fast.start, fast.exact), (3600.0, false));
        let exact = playback(&parse(&["--exact-start", "3600", "--duration", "10"]).unwrap());
        assert_eq!(
            (exact.start, exact.exact, exact.duration),
            (3600.0, true, Some(10.0))
        );
    }
}
