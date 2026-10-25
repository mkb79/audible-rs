//! The HLS half of the FairPlay protocol: parse the playlists a grant points
//! to, walk from the master playlist to the `skd://` key URIs, and derive the
//! content identifier the key exchange runs on.
//!
//! Playlist URLs are signed (the master carries its token in the path), so
//! they never reach an error message: HTTP errors drop their URL.

use std::collections::VecDeque;

use url::Url;

use super::FairPlayError;

/// Playlists fetched at most by one [`discover`] (the master included).
const MAX_PLAYLISTS: usize = 8;
/// How deep [`discover`] follows playlist references below the master.
const MAX_DEPTH: usize = 2;
/// Larger answers are not a playlist this walk should read.
const MAX_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;

/// One `#EXT-X-STREAM-INF` variant of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// The variant playlist URI as written (usually relative).
    pub uri: String,
    /// `BANDWIDTH` (peak, bits per second).
    pub bandwidth: Option<u64>,
    /// `AVERAGE-BANDWIDTH` (bits per second).
    pub average_bandwidth: Option<u64>,
    /// `CODECS`, e.g. `mp4a.40.42`.
    pub codecs: Option<String>,
    /// `AUDIO` — the rendition group the variant plays with.
    pub audio_group: Option<String>,
}

/// One `#EXT-X-MEDIA` rendition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    /// `TYPE` (e.g. `AUDIO`).
    pub media_type: Option<String>,
    /// `GROUP-ID`.
    pub group_id: Option<String>,
    /// `NAME`.
    pub name: Option<String>,
    /// `LANGUAGE`.
    pub language: Option<String>,
    /// `CHANNELS` (e.g. `2`, or `16/JOC` for Atmos).
    pub channels: Option<String>,
    /// `URI` of the rendition playlist, when it has its own.
    pub uri: Option<String>,
}

/// A parsed playlist — a master (variants, renditions, maybe session keys)
/// or a media playlist (keys only, for this walk).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Playlist {
    /// The `#EXT-X-STREAM-INF` variants.
    pub variants: Vec<Variant>,
    /// The `#EXT-X-MEDIA` renditions.
    pub renditions: Vec<Rendition>,
    /// The distinct `skd://` key URIs of `#EXT-X-KEY` and
    /// `#EXT-X-SESSION-KEY`, in order of appearance.
    pub key_uris: Vec<String>,
}

impl Playlist {
    /// The playlists this one refers to: the variants, then the renditions
    /// that have their own URI.
    pub fn children(&self) -> impl Iterator<Item = &str> {
        self.variants
            .iter()
            .map(|variant| variant.uri.as_str())
            .chain(self.renditions.iter().filter_map(|r| r.uri.as_deref()))
    }
}

/// Parses an HLS playlist. Only the tags the FairPlay walk needs are read;
/// everything else (segments, other tags) is skipped.
pub fn parse(text: &str) -> Result<Playlist, FairPlayError> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(FairPlayError::Playlist("not an HLS playlist"));
    }
    let mut playlist = Playlist::default();
    // A `#EXT-X-STREAM-INF` describes the URI on the next non-tag line.
    let mut pending: Option<Variant> = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(Variant {
                uri: String::new(),
                bandwidth: number(attributes, "BANDWIDTH"),
                average_bandwidth: number(attributes, "AVERAGE-BANDWIDTH"),
                codecs: attribute(attributes, "CODECS"),
                audio_group: attribute(attributes, "AUDIO"),
            });
        } else if let Some(attributes) = line.strip_prefix("#EXT-X-MEDIA:") {
            playlist.renditions.push(Rendition {
                media_type: attribute(attributes, "TYPE"),
                group_id: attribute(attributes, "GROUP-ID"),
                name: attribute(attributes, "NAME"),
                language: attribute(attributes, "LANGUAGE"),
                channels: attribute(attributes, "CHANNELS"),
                uri: attribute(attributes, "URI"),
            });
        } else if let Some(attributes) = line
            .strip_prefix("#EXT-X-KEY:")
            .or_else(|| line.strip_prefix("#EXT-X-SESSION-KEY:"))
        {
            // `METHOD=NONE` has no URI, and a non-`skd` URI is not FairPlay.
            if let Some(uri) = attribute(attributes, "URI").filter(|uri| uri.starts_with("skd://"))
                && !playlist.key_uris.contains(&uri)
            {
                playlist.key_uris.push(uri);
            }
        } else if !line.starts_with('#')
            && let Some(mut variant) = pending.take()
        {
            variant.uri = line.to_owned();
            playlist.variants.push(variant);
        }
    }
    Ok(playlist)
}

/// The FairPlay content identifier: the host of `skd://<id>[/…]`. The key
/// requests AVFoundation raises and the stored keys are matched on it.
pub fn content_id(key_uri: &str) -> Option<&str> {
    let rest = key_uri.strip_prefix("skd://")?;
    let id = rest.split(['/', '?', '#']).next()?;
    (!id.is_empty()).then_some(id)
}

/// What [`discover`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// The master playlist, with its variants and renditions.
    pub master: Playlist,
    /// Every distinct `skd://` key URI found, in discovery order.
    pub key_uris: Vec<String>,
}

/// A plain HTTP client for the playlists: no auth (the URLs are signed), no
/// compression (the signed CDN URLs are picky about `Accept-Encoding`, as on
/// the Widevine path), and the API client's timeouts.
pub fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .connect_timeout(crate::api::client::CONNECT_TIMEOUT)
        .read_timeout(crate::api::client::READ_TIMEOUT)
        .build()
}

/// Fetches the master playlist at `master_url` and collects the FairPlay key
/// URIs. Session keys in the master name every key up front; without them
/// the walk follows the variant and rendition playlists, within eight
/// fetches and two levels.
pub async fn discover(
    http: &reqwest::Client,
    master_url: &str,
) -> Result<Discovery, FairPlayError> {
    let base =
        Url::parse(master_url).map_err(|_| FairPlayError::Playlist("invalid playlist URL"))?;
    if !is_http(&base) {
        return Err(FairPlayError::Playlist("the playlist URL is not HTTP(S)"));
    }
    let master = parse(&fetch(http, &base).await?)?;
    let mut key_uris = master.key_uris.clone();
    if key_uris.is_empty() {
        let mut fetched = 1;
        let mut seen = vec![base.clone()];
        let mut queue: VecDeque<(Url, usize)> = children(&master, &base, 1).collect();
        while let Some((url, depth)) = queue.pop_front() {
            if fetched >= MAX_PLAYLISTS {
                break;
            }
            if depth > MAX_DEPTH || !is_http(&url) || seen.contains(&url) {
                continue;
            }
            seen.push(url.clone());
            let playlist = parse(&fetch(http, &url).await?)?;
            fetched += 1;
            for uri in &playlist.key_uris {
                if !key_uris.contains(uri) {
                    key_uris.push(uri.clone());
                }
            }
            queue.extend(children(&playlist, &url, depth + 1));
        }
    }
    if key_uris.is_empty() {
        return Err(FairPlayError::Playlist(
            "no skd:// key URI within the playlist search limit",
        ));
    }
    Ok(Discovery { master, key_uris })
}

/// A playlist's references, resolved against its own URL.
fn children<'a>(
    playlist: &'a Playlist,
    base: &'a Url,
    depth: usize,
) -> impl Iterator<Item = (Url, usize)> + 'a {
    playlist
        .children()
        .filter_map(move |uri| base.join(uri).ok())
        .map(move |url| (url, depth))
}

fn is_http(url: &Url) -> bool {
    matches!(url.scheme(), "https" | "http")
}

/// GETs one playlist. Errors drop the URL: it is signed.
async fn fetch(http: &reqwest::Client, url: &Url) -> Result<String, FairPlayError> {
    let response = http
        .get(url.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PLAYLIST_BYTES as u64)
    {
        return Err(FairPlayError::Playlist("playlist too large"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(reqwest::Error::without_url)?;
    if bytes.len() > MAX_PLAYLIST_BYTES {
        return Err(FairPlayError::Playlist("playlist too large"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| FairPlayError::Playlist("playlist is not UTF-8"))
}

/// One attribute of an HLS attribute list (`KEY=VALUE,KEY="VALUE",…`).
/// Quoted values may contain commas (`CODECS="mp4a.40.2,ec+3"`).
fn attribute(list: &str, name: &str) -> Option<String> {
    let mut rest = list;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"')?;
                (&quoted[..end], quoted[end + 1..].trim_start_matches(','))
            }
            None => after.split_once(',').unwrap_or((after, "")),
        };
        if key.trim() == name {
            return Some(value.to_owned());
        }
        rest = next;
    }
    None
}

fn number(list: &str, name: &str) -> Option<u64> {
    attribute(list, name)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-VERSION:6\n\
        #EXT-X-INDEPENDENT-SEGMENTS\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"atmos\",NAME=\"Atmos\",LANGUAGE=\"en\",CHANNELS=\"16/JOC\",URI=\"atmos/index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=68000,AVERAGE-BANDWIDTH=64000,CODECS=\"mp4a.40.42\"\n\
        title_64-xhe.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"mp4a.40.2,ec+3\",AUDIO=\"atmos\"\n\
        \n\
        title_atmos.m3u8\n";

    const MEDIA: &str = "#EXTM3U\n\
        #EXT-X-TARGETDURATION:10\n\
        #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://00000000-0000-4000-8000-000000000001\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
        #EXTINF:10.0,\n\
        segment_0.mp4\n\
        #EXT-X-KEY:METHOD=NONE\n\
        #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.invalid/k\"\n\
        #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://00000000-0000-4000-8000-000000000001\"\n\
        #EXTINF:10.0,\n\
        segment_1.mp4\n";

    #[test]
    fn parses_variants_and_renditions_of_a_master() {
        let playlist = parse(MASTER).unwrap();
        assert_eq!(playlist.variants.len(), 2);
        let xhe = &playlist.variants[0];
        assert_eq!(xhe.uri, "title_64-xhe.m3u8");
        assert_eq!(xhe.bandwidth, Some(68000));
        assert_eq!(xhe.average_bandwidth, Some(64000));
        assert_eq!(xhe.codecs.as_deref(), Some("mp4a.40.42"));
        let atmos = &playlist.variants[1];
        // A quoted value keeps its comma; a blank line does not end the tag.
        assert_eq!(atmos.codecs.as_deref(), Some("mp4a.40.2,ec+3"));
        assert_eq!(atmos.audio_group.as_deref(), Some("atmos"));
        assert_eq!(atmos.uri, "title_atmos.m3u8");
        assert_eq!(playlist.renditions[0].channels.as_deref(), Some("16/JOC"));
        assert!(playlist.key_uris.is_empty());
        assert_eq!(
            playlist.children().collect::<Vec<_>>(),
            ["title_64-xhe.m3u8", "title_atmos.m3u8", "atmos/index.m3u8"]
        );
    }

    #[test]
    fn collects_only_distinct_fairplay_keys() {
        let playlist = parse(MEDIA).unwrap();
        assert_eq!(
            playlist.key_uris,
            ["skd://00000000-0000-4000-8000-000000000001"]
        );
        assert!(playlist.variants.is_empty());
    }

    #[test]
    fn session_keys_count_as_keys() {
        let text = "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://abc\"\n";
        assert_eq!(parse(text).unwrap().key_uris, ["skd://abc"]);
    }

    #[test]
    fn rejects_what_is_not_a_playlist() {
        assert!(matches!(
            parse("<html>denied</html>"),
            Err(FairPlayError::Playlist(_))
        ));
    }

    #[test]
    fn content_id_is_the_skd_host() {
        assert_eq!(content_id("skd://abc-123"), Some("abc-123"));
        assert_eq!(content_id("skd://abc-123/path?x=1"), Some("abc-123"));
        assert_eq!(content_id("skd://"), None);
        assert_eq!(content_id("https://abc-123"), None);
    }

    #[test]
    fn attribute_lists_with_and_without_quotes() {
        let list = "A=1,B=\"x,y\",C=plain";
        assert_eq!(attribute(list, "A").as_deref(), Some("1"));
        assert_eq!(attribute(list, "B").as_deref(), Some("x,y"));
        assert_eq!(attribute(list, "C").as_deref(), Some("plain"));
        assert_eq!(attribute(list, "D"), None);
        assert_eq!(number("BANDWIDTH=abc", "BANDWIDTH"), None);
    }
}
