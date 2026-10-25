//! `audible fairplay` — **experimental**, macOS only (AUD-321): download,
//! stream and play titles Audible serves with FairPlay, through AVFoundation
//! (see [`crate::fairplay`]).
//!
//! Kept apart from the rest of the CLI on purpose. It writes nothing to the
//! database — no download records, no stored licenses, no library sync — and
//! keeps its state in its own store (see [`store`]). Only the audio is
//! handled; covers, chapters and PDFs stay with `audible download`.
//!
//! Like the library module, the command exists on macOS only.

mod download;
mod play;
mod store;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use clap::{Arg, ArgAction};

use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::fairplay::{Consumption, LicenseOptions, Quality};

/// `audible fairplay`.
pub struct FairplayCommand;

#[async_trait::async_trait]
impl super::Command for FairplayCommand {
    fn name(&self) -> &'static str {
        "fairplay"
    }

    fn clap(&self) -> clap::Command {
        clap::Command::new(self.name())
            .about("Experimental: FairPlay downloads and playback (macOS)")
            .long_about(
                "Experimental, macOS only: download, stream and play titles Audible serves \
                 with FairPlay, through Apple's AVFoundation. A FairPlay download is an \
                 AVFoundation package that only plays through AVFoundation (e.g. `audible \
                 fairplay play`): there is no m4b and no export. Nothing is written to the \
                 library database; the downloads are tracked in their own store.",
            )
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(download::command())
            .subcommand(play::stream_command())
            .subcommand(play::play_command())
    }

    async fn run(&self, ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
        ctx.warn(
            "fairplay_experimental",
            "audible fairplay is experimental: its options, store and behavior may change",
        );
        match matches.subcommand() {
            Some(("download", sub)) => download::run(ctx, sub).await,
            Some(("stream", sub)) => play::stream(ctx, sub).await,
            Some(("play", sub)) => play::play(ctx, sub).await,
            _ => unreachable!("clap requires a subcommand"),
        }
    }
}

/// `--codec` and `--spatial`, shared by `download` and `stream`.
fn codec_args() -> [Arg; 2] {
    [
        Arg::new("codec")
            .long("codec")
            .value_name("CODEC")
            .value_parser(["aac", "xhe"])
            .default_value("aac")
            .help("Audio codec to offer: aac (AAC-LC) or xhe (also xHE-AAC)"),
        Arg::new("spatial")
            .long("spatial")
            .action(ArgAction::SetTrue)
            .help("Also offer Dolby Atmos (E-AC-3 JOC)"),
    ]
}

/// `--dir`: the FairPlay store, shared by `download` and `play`.
fn dir_arg() -> Arg {
    Arg::new("dir").long("dir").value_name("DIR").help(
        "FairPlay store for the packages, index and keys (default: \
             ~/Library/Application Support/audible-rs/FairPlay); must lie outside \
             the download dir",
    )
}

/// What a licenserequest asks for, from the shared flags.
fn license_options(matches: &clap::ArgMatches, consumption: Consumption) -> LicenseOptions {
    LicenseOptions {
        consumption,
        quality: match matches
            .try_get_one::<String>("quality")
            .ok()
            .flatten()
            .map(String::as_str)
        {
            Some("normal") => Quality::Normal,
            _ => Quality::High,
        },
        xhe: matches.get_one::<String>("codec").map(String::as_str) == Some("xhe"),
        spatial: matches.get_flag("spatial"),
    }
}

/// Resolves `--asin`/`--title` against the library (read only). With
/// `library_only`, an `--asin` the library does not hold is refused, as
/// `audible download` does without `--allow-external`.
async fn resolve_titles(
    ctx: &Ctx,
    marketplace: &str,
    matches: &clap::ArgMatches,
    library_only: bool,
) -> Result<Vec<String>> {
    let asins = crate::commands::strings(matches, "asin");
    let titles = crate::commands::strings(matches, "title");
    let db = ctx.open_library_db().await?;
    if library_only {
        for asin in &asins {
            if db
                .find_title(asin.clone(), marketplace.to_owned())
                .await?
                .is_none()
            {
                bail!(
                    "{asin} is not in your library — pass --allow-external if a \
                     subscription covers it"
                );
            }
        }
    }
    crate::commands::items::resolve_asins(
        &db,
        marketplace,
        asins,
        titles,
        crate::commands::items::PodcastMode::ItemsOnly,
    )
    .await
}

/// The one title `stream` or `play` acts on.
async fn single_title(
    ctx: &Ctx,
    marketplace: &str,
    matches: &clap::ArgMatches,
    library_only: bool,
) -> Result<String> {
    if !matches.contains_id("asin") && !matches.contains_id("title") {
        bail!("specify the title: --asin <ASIN> or --title <QUERY>");
    }
    let mut asins = resolve_titles(ctx, marketplace, matches, library_only).await?;
    match asins.len() {
        1 => Ok(asins.remove(0)),
        0 => bail!("the requested title resolved to nothing"),
        n => bail!("{n} titles match — pick one"),
    }
}

/// The library title of `asin`, for messages; `None` when unknown.
async fn title_of(ctx: &Ctx, marketplace: &str, asin: &str) -> Option<String> {
    let db = ctx.open_library_db().await.ok()?;
    db.find_title(asin.to_owned(), marketplace.to_owned())
        .await
        .ok()
        .flatten()
}

/// The SPC → CKC exchange the drivers call, run on the tokio runtime from
/// inside `block_in_place`.
fn ckc_exchange<'a>(
    handle: &'a tokio::runtime::Handle,
    client: &'a Client,
    marketplace: &'a str,
    asin: &'a str,
    license_id: Option<&'a str>,
    consumption: Consumption,
) -> impl FnMut(&str, &[u8]) -> Result<Vec<u8>, String> + 'a {
    move |_content_id, spc| {
        handle
            .block_on(crate::fairplay::request_ckc(
                client,
                marketplace,
                asin,
                spc,
                license_id,
                consumption,
            ))
            .map_err(|error| error.to_string())
    }
}

/// A flag Ctrl-C sets, so a download or playback stops cleanly (a cancelled
/// background download would otherwise finish unrecorded in the system
/// cache).
fn stop_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&flag);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            set.store(true, Ordering::Relaxed);
        }
    });
    flag
}

/// `H:MM:SS`.
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats_hours_minutes_seconds() {
        assert_eq!(clock(0.0), "0:00:00");
        assert_eq!(clock(3725.9), "1:02:05");
        assert_eq!(clock(-3.0), "0:00:00");
    }

    #[test]
    fn license_options_follow_the_flags() {
        let cmd = clap::Command::new("t")
            .arg(
                Arg::new("quality")
                    .long("quality")
                    .value_parser(["high", "normal"])
                    .default_value("high"),
            )
            .args(codec_args());
        let matches = cmd
            .try_get_matches_from(["t", "--quality", "normal", "--codec", "xhe", "--spatial"])
            .unwrap();
        let options = license_options(&matches, Consumption::Download);
        assert_eq!(options.quality, Quality::Normal);
        assert!(options.xhe && options.spatial);

        // `stream` has no --quality: the streaming license asks for the
        // bitrate ladder instead.
        let stream = clap::Command::new("t").args(codec_args());
        let matches = stream.try_get_matches_from(["t"]).unwrap();
        let options = license_options(&matches, Consumption::Streaming);
        assert_eq!(options.quality, Quality::High);
        assert!(!options.xhe && !options.spatial);
    }
}
