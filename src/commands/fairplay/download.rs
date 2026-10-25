//! `audible fairplay download` — `audible download` for the audio, FairPlay
//! only: license, AVFoundation package download with persistable keys, and
//! an entry in the FairPlay store. No database writes, no fallback to another
//! format.
//!
//! AVFoundation downloads into the system cache; the finished package then
//! moves into the FairPlay store (see [`super::store`]).

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use clap::{Arg, ArgAction};

use super::store::{Entry, Index, Store};
use crate::api::client::Client;
use crate::config::ctx::Ctx;
use crate::fairplay::driver::{self, DownloadJob, DownloadProgress};
use crate::fairplay::{self, Consumption, LicenseOptions};
use crate::output::Output;

/// A request kind no download record carries: asking the library for the
/// titles that miss it lists every downloadable title. The FairPlay index,
/// not the download records, then decides what is missing.
const NEVER_RECORDED: &str = "fairplay";

pub(super) fn command() -> clap::Command {
    let cmd = clap::Command::new("download")
        .about("Download titles as FairPlay packages (experimental, macOS)")
        .long_about(
            "Download the audio of titles as FairPlay packages (experimental, macOS only). \
             Only FairPlay is requested; a title Audible does not serve with FairPlay fails \
             instead of falling back to another format. The finished package moves from the \
             system cache into the FairPlay store (~/Library/Application \
             Support/audible-rs/FairPlay, or --dir), next to its keys. Play it with \
             `audible fairplay play`.",
        );
    crate::commands::items::item_source_args(cmd)
        .arg(
            Arg::new("missing")
                .long("missing")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["asin", "title"])
                .help("Every library title without a FairPlay download yet"),
        )
        .arg(
            Arg::new("include_archived")
                .long("include-archived")
                .action(ArgAction::SetTrue)
                .help("With --missing: include archived titles"),
        )
        .arg(
            Arg::new("allow_external")
                .long("allow-external")
                .action(ArgAction::SetTrue)
                .help(
                    "Allow --asin values that are not in your library (needs a \
                     subscription that covers the title)",
                ),
        )
        .arg(
            Arg::new("quality")
                .long("quality")
                .value_name("QUALITY")
                .value_parser(["high", "normal"])
                .default_value("high")
                .help("Audio quality"),
        )
        .args(super::codec_args())
        .arg(super::dir_arg())
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .help("Download again even if the title is already in the FairPlay store"),
        )
        .arg(
            Arg::new("license_only")
                .long("license-only")
                .action(ArgAction::SetTrue)
                .help("Only request the license and print what was granted, with its variants"),
        )
}

pub(super) async fn run(ctx: &Ctx, matches: &clap::ArgMatches) -> Result<()> {
    let missing = matches.get_flag("missing");
    if !missing && !matches.contains_id("asin") && !matches.contains_id("title") {
        bail!("specify what to download: --asin <ASIN>, --title <QUERY>, or --missing");
    }
    // Not expressible via clap `requires`: SetTrue flags count as present
    // through their default.
    if matches.get_flag("include_archived") && !missing {
        bail!("--include-archived only applies to the --missing selection");
    }
    let marketplace = ctx.marketplace_single()?;
    let options = super::license_options(matches, Consumption::Download);
    let store = Store::open(ctx, matches.get_one::<String>("dir"))?;
    let mut index = store.load()?;

    let asins = if missing {
        missing_titles(
            ctx,
            &marketplace,
            &index,
            matches.get_flag("include_archived"),
        )
        .await?
    } else {
        super::resolve_titles(
            ctx,
            &marketplace,
            matches,
            !matches.get_flag("allow_external"),
        )
        .await?
    };
    if asins.is_empty() {
        if !missing {
            crate::commands::items::require_nonempty(&asins, "items")?;
        }
        eprintln!("no items to download");
        return Ok(());
    }

    let client = ctx.client().await?;
    if matches.get_flag("license_only") {
        return license_only(ctx, client, &marketplace, &asins, &options).await;
    }

    let account = ctx.account_name()?;
    let force = matches.get_flag("force");
    let show_progress = !matches.get_flag("quiet") && console::Term::stderr().is_term();
    let handle = tokio::runtime::Handle::current();
    let stop = super::stop_flag();
    let single = asins.len() == 1;
    let mut rows = Vec::new();
    let mut failed = 0;
    for asin in &asins {
        if stop.load(Ordering::Relaxed) {
            bail!("interrupted");
        }
        let previous = index.get(&marketplace, asin).cloned();
        if let Some(entry) = &previous
            && !force
        {
            if entry.package.exists() {
                eprintln!("skipping {asin} — already downloaded (use --force)");
                continue;
            }
            eprintln!("{asin}: its package is gone (macOS may have purged it) — downloading again");
        }
        let title = super::title_of(ctx, &marketplace, asin).await;
        let target = Target {
            client,
            handle: &handle,
            store: &store,
            marketplace: &marketplace,
            account: &account,
            options: &options,
            show_progress,
            stop: &stop,
        };
        match download_one(&target, asin, title).await {
            Ok(entry) => {
                // The new package replaced the old one at the same path; an
                // entry pointing elsewhere (an earlier layout) is cleaned up.
                if let Some(old) = previous.filter(|old| old.package != entry.package)
                    && old.package.extension().is_some_and(|ext| ext == "movpkg")
                    && let Err(error) = super::store::remove_package(&old.package)
                {
                    eprintln!("warning: {error:#}");
                }
                rows.push(vec![
                    asin.clone(),
                    entry.content_format.clone().unwrap_or_default(),
                    entry.package.display().to_string(),
                ]);
                index.entries.insert(Index::key(&marketplace, asin), entry);
                store.save(&index)?;
            }
            Err(error) if single => return Err(error),
            Err(error) => {
                eprintln!("error for {asin}: {error:#}");
                failed += 1;
            }
        }
    }
    if !rows.is_empty() {
        ctx.print(&Output::table(vec!["asin", "format", "package"], rows));
    }
    if failed > 0 {
        bail!("{failed} of {} item(s) failed", asins.len());
    }
    Ok(())
}

/// What every item of a run shares.
struct Target<'a> {
    client: &'a Client,
    handle: &'a tokio::runtime::Handle,
    store: &'a Store,
    marketplace: &'a str,
    account: &'a str,
    options: &'a LicenseOptions,
    show_progress: bool,
    stop: &'a AtomicBool,
}

async fn download_one(target: &Target<'_>, asin: &str, title: Option<String>) -> Result<Entry> {
    let grant =
        fairplay::request_license(target.client, target.marketplace, asin, target.options).await?;
    let certificate = match &grant.certificate {
        Some(certificate) => certificate.clone(),
        None => fairplay::request_certificate(target.client, target.marketplace).await?,
    };
    let bar = target.show_progress.then(|| {
        let bar = indicatif::ProgressBar::new(1000);
        bar.set_style(
            indicatif::ProgressStyle::with_template("{msg} [{bar:30}] {percent:>3}%")
                .expect("valid template")
                .progress_chars("=> "),
        );
        bar.set_message(asin.to_owned());
        bar
    });

    let job = DownloadJob {
        hls_url: &grant.hls_url,
        title: title.as_deref().unwrap_or(asin),
        certificate: &certificate,
    };
    // AVFoundation runs on the main thread's run loop: the driver blocks it
    // and calls back for each SPC, which goes to the license server on the
    // tokio runtime.
    let result = tokio::task::block_in_place(|| {
        let mut ckc = super::ckc_exchange(
            target.handle,
            target.client,
            target.marketplace,
            asin,
            grant.license_id.as_deref(),
            Consumption::Download,
        );
        let mut progress = |event: DownloadProgress| {
            if let (Some(bar), DownloadProgress::Fraction(done)) = (&bar, event) {
                bar.set_position((done * 1000.0) as u64);
            }
        };
        driver::download(&job, &mut ckc, &mut progress, &|| {
            target.stop.load(Ordering::Relaxed)
        })
    });
    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }
    let done = result.map_err(|error| anyhow!("{asin}: {error}"))?;

    // Keys first: a package without its keys is useless, keys without a
    // package are just a few bytes.
    let keys = Store::keys_name(target.marketplace, asin);
    target.store.write_keys(&keys, &done.keys)?;
    let package = match target
        .store
        .adopt_package(target.marketplace, asin, &done.package)
    {
        Ok(package) => package,
        Err(error) => {
            // It would otherwise linger in the system cache, unrecorded.
            let _ = super::store::remove_package(&done.package);
            return Err(error);
        }
    };
    Ok(Entry {
        asin: asin.to_owned(),
        marketplace: target.marketplace.to_owned(),
        account: target.account.to_owned(),
        title,
        package,
        keys,
        codec: grant.codec,
        content_format: grant.content_format,
        quality: target.options.quality.api_value().to_owned(),
        xhe: target.options.xhe,
        spatial: target.options.spatial,
        granted_right: grant.granted_right,
        access_expiry: grant.access_expiry,
        downloaded_at: crate::timefmt::now_iso(),
    })
}

/// `--missing`: the library's downloadable titles (read only) without a
/// package in the FairPlay store.
async fn missing_titles(
    ctx: &Ctx,
    marketplace: &str,
    index: &Index,
    include_archived: bool,
) -> Result<Vec<String>> {
    let db = ctx.open_library_db().await?;
    let titles = db
        .missing_download_asins(
            vec![marketplace.to_owned()],
            vec!["audio".to_owned()],
            vec![NEVER_RECORDED.to_owned()],
            include_archived,
            false,
            false,
        )
        .await?;
    Ok(titles
        .into_iter()
        .filter(|asin| {
            !index
                .get(marketplace, asin)
                .is_some_and(|entry| entry.package.exists())
        })
        .collect())
}

/// `--license-only`: requests the license and prints what was granted, with
/// the variants of the master playlist.
async fn license_only(
    ctx: &Ctx,
    client: &Client,
    marketplace: &str,
    asins: &[String],
    options: &LicenseOptions,
) -> Result<()> {
    let http = fairplay::hls::http_client()?;
    let mut rows = Vec::new();
    for asin in asins {
        let grant = fairplay::request_license(client, marketplace, asin, options).await?;
        let variants = match fairplay::discover(&http, &grant.hls_url).await {
            Ok(found) => describe_variants(&found.master),
            Err(error) => format!("({error})"),
        };
        rows.push(vec![
            asin.clone(),
            grant.codec.unwrap_or_default(),
            grant.content_format.unwrap_or_default(),
            grant.granted_right.unwrap_or_default(),
            if grant.certificate.is_some() {
                "inline"
            } else {
                "separate"
            }
            .to_owned(),
            variants,
        ]);
    }
    ctx.print(&Output::table(
        vec![
            "asin",
            "codec",
            "format",
            "right",
            "certificate",
            "variants",
        ],
        rows,
    ));
    Ok(())
}

/// The variants of a master playlist, e.g. `mp4a.40.42 64 kbps; …`, with the
/// channel layouts of its renditions.
fn describe_variants(master: &fairplay::Playlist) -> String {
    let mut parts: Vec<String> = master
        .variants
        .iter()
        .map(|variant| {
            let codecs = variant.codecs.as_deref().unwrap_or("?");
            match variant.average_bandwidth.or(variant.bandwidth) {
                Some(bits) => format!("{codecs} {} kbps", bits / 1000),
                None => codecs.to_owned(),
            }
        })
        .collect();
    parts.extend(master.renditions.iter().filter_map(|rendition| {
        rendition.channels.as_deref().map(|channels| {
            format!(
                "{} {channels} ch",
                rendition.name.as_deref().unwrap_or("audio")
            )
        })
    }));
    if parts.is_empty() {
        "single media playlist".to_owned()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_variants_and_renditions() {
        let master = fairplay::hls::parse(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Atmos\",CHANNELS=\"16/JOC\",URI=\"a.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=68000,AVERAGE-BANDWIDTH=64000,CODECS=\"mp4a.40.42\"\n\
             x.m3u8\n",
        )
        .unwrap();
        assert_eq!(
            describe_variants(&master),
            "mp4a.40.42 64 kbps; Atmos 16/JOC ch"
        );
        let media = fairplay::hls::parse("#EXTM3U\n#EXTINF:10,\ns.mp4\n").unwrap();
        assert_eq!(describe_variants(&media), "single media playlist");
    }
}
